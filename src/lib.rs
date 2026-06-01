pub mod error;

use async_channel::{Sender, TrySendError};
use error::{Error, Result};
use log::*;
use std::future::Future;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadHalf};
use tokio::net::{TcpStream, ToSocketAddrs};

/// 基于 actor 模式的异步 TCP 客户端。
///
/// 泛型参数 `T` 为底层流类型，通常为 [`TcpStream`] 或经过 TLS 包装的流。
///
/// # 线程安全
///
/// 所有方法均通过不可变引用操作，客户端应包装在 [`Arc`] 中使用。
pub struct TcpClient<T> {
    /// 标记连接是否已断开，防止重复发送 Disconnect 消息。
    disconnect: AtomicBool,
    /// 向 writer 任务发送写命令的 channel 发送端。
    sender: Sender<State>,
    /// 标记泛型 `T` 的所有权，零大小字段，无运行时开销。
    _ph: PhantomData<T>,
}

// ===== TcpStream 特化方法 =====

impl TcpClient<TcpStream> {
    /// 连接到指定地址，返回 `Arc` 包装的客户端。
    ///
    /// # 参数
    ///
    /// - `addr`：目标地址，实现了 [`ToSocketAddrs`]。
    /// - `input`：reader 闭包，在独立的 tokio 任务中执行，接收 token、client 句柄和读半端。
    /// - `token`：传递给 reader 闭包的用户数据。
    ///
    /// # Reader 闭包的返回值
    ///
    /// - `Ok(true)`：闭包结束后触发断开连接。
    /// - `Ok(false)`：保持连接不断开。
    /// - `Err(_)`：记录错误日志并触发断开连接。
    #[inline]
    pub async fn connect<
        T: ToSocketAddrs,
        F: Future<Output = anyhow::Result<bool>> + Send + 'static,
        A: Send + 'static,
    >(
        addr: T,
        input: impl FnOnce(A, Arc<TcpClient<TcpStream>>, ReadHalf<TcpStream>) -> F + Send + 'static,
        token: A,
    ) -> Result<Arc<TcpClient<TcpStream>>> {
        let stream = TcpStream::connect(addr).await?;
        let target = stream.peer_addr()?;
        Self::init(input, token, stream, target)
    }
}

/// Writer 任务从 channel 中消费的状态消息。
///
/// 四种变体对应四种 TCP 写操作，按 FIFO 顺序在 writer 任务中执行。
pub enum State {
    /// 关闭 writer 半端，退出 writer 任务。
    Disconnect,
    /// 写入数据，不刷新（依赖 TCP 栈缓冲 / Nagle 算法合并小包）。
    Send(Vec<u8>),
    /// 写入数据后立即刷新，确保数据尽快到达对端。
    SendFlush(Vec<u8>),
    /// 仅刷新写缓冲区，不写入新数据。
    Flush,
}

// ===== 泛型方法实现 =====

impl<T> TcpClient<T>
where
    T: AsyncRead + AsyncWrite + Send + Sync + 'static,
{
    /// 连接到目标地址，在建立原始 TCP 连接后通过 `stream_init` 对连接进行包装。
    ///
    /// 典型用法是在此进行 TLS 握手，将 [`TcpStream`] 转换为 TLS 流类型。
    ///
    /// # 参数
    ///
    /// - `addr`：目标地址。
    /// - `stream_init`：流初始化闭包，在连接建立后、actor 循环启动前执行。
    /// - `input`：reader 闭包，同 [`TcpClient::connect`]。
    /// - `token`：传递给 reader 闭包的用户数据。
    #[inline]
    pub async fn connect_stream_type<
        H: ToSocketAddrs,
        F: Future<Output = anyhow::Result<bool>> + Send + 'static,
        S: Future<Output = anyhow::Result<T>> + Send + 'static,
        A: Send + 'static,
    >(
        addr: H,
        stream_init: impl FnOnce(TcpStream) -> S + Send + 'static,
        input: impl FnOnce(A, Arc<TcpClient<T>>, ReadHalf<T>) -> F + Send + 'static,
        token: A,
    ) -> Result<Arc<TcpClient<T>>> {
        let stream = TcpStream::connect(addr).await?;
        let target = stream.peer_addr()?;
        let stream = stream_init(stream).await?;
        Self::init(input, token, stream, target)
    }

    /// 核心初始化逻辑：拆分 TCP 流，创建 channel，启动 reader 和 writer 两个 tokio 任务。
    ///
    /// # 架构
    ///
    /// ```text
    /// ┌──────────────┐     State 消息     ┌──────────────┐
    /// │  用户代码     │ ─────────────────→ │  writer 任务  │ ──→ write half
    /// │ (send/flush) │    async_channel   │              │
    /// └──────────────┘    (bounded 4096)  └──────────────┘
    ///         │                                      │
    ///         │    Arc<TcpClient> 克隆传递            │
    ///         ▼                                      │
    /// ┌──────────────┐                              │
    /// │  reader 任务  │ ←──────── read half ─────────┘
    /// │  (用户闭包)   │
    /// └──────────────┘
    /// ```
    #[inline]
    fn init<F: Future<Output = anyhow::Result<bool>> + Send + 'static, A: Send + 'static>(
        f: impl FnOnce(A, Arc<TcpClient<T>>, ReadHalf<T>) -> F + Send + 'static,
        token: A,
        stream: T,
        target: SocketAddr,
    ) -> Result<Arc<TcpClient<T>>> {
        // 将 TCP 流拆分为独立的读半端和写半端，两者可在不同任务中并发操作
        let (reader, mut writer) = tokio::io::split(stream);

        // 有界 channel：容量 4096，平衡内存占用与吞吐量
        let (tx, rx) = async_channel::bounded(4096);

        let client = Arc::new(TcpClient {
            disconnect: AtomicBool::new(false),
            sender: tx,
            _ph: PhantomData,
        });

        // ---- Reader 任务：执行用户的读取逻辑 ----
        let read_client = client.clone();
        tokio::spawn(async move {
            let disconnect_client = read_client.clone();
            // 调用用户闭包，Err 时自动触发断开
            let need_disconnect = f(token, read_client, reader).await.unwrap_or_else(|err| {
                error!("reader error: {}", err);
                true
            });

            if need_disconnect {
                if let Err(er) = disconnect_client.disconnect().await {
                    error!("disconnect to {} err: {}", target, er);
                } else {
                    debug!("disconnect to {}", target)
                }
            } else {
                debug!("{} reader is close", target);
            }
        });

        // ---- Writer 任务：顺序消费写命令 ----
        tokio::spawn(async move {
            // 复用写缓冲区，避免每次写入都重新分配内存
            let mut buf: Vec<u8> = Vec::with_capacity(8192);
            loop {
                // 阻塞等待下一个写命令
                let state = match rx.recv().await {
                    Ok(s) => s,
                    Err(_) => return, // channel 关闭，writer 退出
                };

                match state {
                    State::Disconnect => {
                        // 优雅关闭写入端，发送 FIN 包
                        let _ = writer.shutdown().await;
                        return;
                    }
                    State::Send(data) => {
                        buf.extend_from_slice(&data);
                        // 尽量一次排空通道中所有连续的 Send 消息，合并成一次 write 系统调用
                        while let Ok(State::Send(more)) = rx.try_recv() {
                            buf.extend_from_slice(&more);
                        }
                        if let Err(e) = writer.write_all(&buf).await {
                            error!("write to {} err: {}", target, e);
                            return;
                        }
                        // 安全：数据已写入，仅重置长度，保留已分配的容量
                        unsafe {
                            buf.set_len(0);
                        }
                    }
                    State::SendFlush(data) => {
                        buf.extend_from_slice(&data);
                        while let Ok(State::Send(more)) = rx.try_recv() {
                            buf.extend_from_slice(&more);
                        }
                        if let Err(e) = writer.write_all(&buf).await {
                            error!("write to {} err: {}", target, e);
                            return;
                        }
                        unsafe {
                            buf.set_len(0);
                        }
                        // 写入后立即刷新，确保数据发送到对端
                        if let Err(e) = writer.flush().await {
                            error!("flush to {} err: {}", target, e);
                            return;
                        }
                    }
                    State::Flush => {
                        if let Err(e) = writer.flush().await {
                            error!("flush to {} err: {}", target, e);
                            return;
                        }
                    }
                }
            }
        });

        Ok(client)
    }

    /// 检查连接是否已断开。
    ///
    /// 使用 [`Ordering::Relaxed`] 是因为此检查仅为快速失败路径，
    /// 真正的同步由 `async_channel` 保证——即使此处误判为"未断开"，
    /// channel 也会在 writer 退出后正确拒绝发送。
    #[inline]
    fn check_disconnected(&self) -> Result<()> {
        if self.disconnect.load(Ordering::Relaxed) {
            return Err(Error::SendError("Disconnect".to_string()));
        }
        Ok(())
    }

    /// 断开连接。
    ///
    /// 向 writer 任务发送 `Disconnect` 消息，触发优雅关闭（发送 FIN 包）。
    ///
    /// # 幂等性
    ///
    /// 重复调用不会产生副作用——`AtomicBool` 确保只发送一次 `Disconnect` 消息。
    #[inline]
    pub async fn disconnect(&self) -> Result<()> {
        if !self.disconnect.load(Ordering::Relaxed) {
            self.sender.send(State::Disconnect).await?;
            // Release 保证：对 writer 的写入 happens-before 其他线程读到 disconnect=true
            self.disconnect.store(true, Ordering::Release);
        }
        Ok(())
    }

    /// 非阻塞发送数据。
    ///
    /// 通道有空位时立即返回 `Ok(())`；通道满时返回 `Err` 而不挂起当前 task。
    ///
    /// 适用于无法等待的场景，如实时流中丢帧保活。
    #[inline]
    pub fn try_send(&self, buff: Vec<u8>) -> Result<()> {
        self.check_disconnected()?;
        match self.sender.try_send(State::Send(buff)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(Error::SendError("channel full".to_string())),
            Err(TrySendError::Closed(_)) => Err(Error::SendError("channel closed".to_string())),
        }
    }

    /// 异步发送数据（写入 TCP 流但不刷新）。
    ///
    /// 内部使用 `try_send` 快速路径：通道有空位时无需挂起 task；
    /// 仅当通道满时才挂起等待。
    ///
    /// 不刷新意味着数据可能被 TCP 栈缓冲（Nagle 算法），适合高频小包场景。
    /// 如需立即送达，请使用 [`send_all`](TcpClient::send_all)。
    #[inline]
    pub async fn send(&self, buff: Vec<u8>) -> Result<()> {
        self.check_disconnected()?;
        match self.sender.try_send(State::Send(buff)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(state)) => Ok(self.sender.send(state).await?),
            Err(TrySendError::Closed(_)) => Err(Error::SendError("channel closed".to_string())),
        }
    }

    /// 异步发送数据后立即刷新到对端。
    ///
    /// 与 [`send`](TcpClient::send) 的区别在于写入后立即调用 `flush`，
    /// 适合需要尽快送达的消息（如请求-响应协议中的请求帧）。
    #[inline]
    pub async fn send_all(&self, buff: Vec<u8>) -> Result<()> {
        self.check_disconnected()?;
        match self.sender.try_send(State::SendFlush(buff)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(state)) => Ok(self.sender.send(state).await?),
            Err(TrySendError::Closed(_)) => Err(Error::SendError("channel closed".to_string())),
        }
    }

    /// 异步刷新写缓冲区，将 TCP 栈中缓冲的数据发送到对端。
    ///
    /// 通常在发送一系列 [`send`](TcpClient::send) 后调用，确保全部数据发出。
    #[inline]
    pub async fn flush(&self) -> Result<()> {
        self.check_disconnected()?;
        match self.sender.try_send(State::Flush) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Ok(self.sender.send(State::Flush).await?),
            Err(TrySendError::Closed(_)) => Err(Error::SendError("channel closed".to_string())),
        }
    }
}

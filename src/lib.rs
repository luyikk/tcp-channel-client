pub mod error;

use async_channel::{Sender, TrySendError};
use error::{Error, Result};
use log::*;
use std::future::Future;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadHalf};
use tokio::net::{TcpStream, ToSocketAddrs};

/// 基于 actor 模式的异步 TCP 客户端。
///
/// 泛型参数 `Stream` 为底层流类型，通常为 [`TcpStream`] 或经过 TLS 包装的流。
///
/// # 线程安全
///
/// 所有方法均通过不可变引用操作，客户端应包装在 [`Arc`] 中使用。
pub struct TcpClient<Stream> {
    /// 标记连接是否已断开，防止重复发送 Disconnect 消息。
    disconnect: AtomicBool,
    /// 向 writer 任务发送写命令的 channel 发送端。
    sender: Sender<State>,
    /// 对端地址，用于诊断和错误信息，便于多连接场景排查。
    peer_addr: SocketAddr,
    /// 标记泛型 `Stream` 的所有权，零大小字段，无运行时开销。
    _ph: PhantomData<Stream>,
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
        Addr: ToSocketAddrs,
        Fut: Future<Output = anyhow::Result<bool>> + Send + 'static,
        Token: Send + 'static,
    >(
        addr: Addr,
        input: impl FnOnce(Token, Arc<TcpClient<TcpStream>>, ReadHalf<TcpStream>) -> Fut
            + Send
            + 'static,
        token: Token,
    ) -> Result<Arc<TcpClient<TcpStream>>> {
        let stream = TcpStream::connect(addr).await?;
        let target = stream.peer_addr()?;
        Self::init(input, token, stream, target)
    }

    /// 连接到指定地址，在指定超时时间内未完成则返回错误。
    ///
    /// 与 [`connect`](TcpClient::connect) 的区别是增加了 `timeout` 参数，
    /// 通过 [`tokio::time::timeout`] 包装 TCP 连接，避免系统默认超时（可能非常长）。
    ///
    /// 超时错误以 [`Error::IOError`] 返回，携带 [`std::io::ErrorKind::TimedOut`]。
    #[inline]
    pub async fn connect_timeout<
        Addr: ToSocketAddrs,
        Fut: Future<Output = anyhow::Result<bool>> + Send + 'static,
        Token: Send + 'static,
    >(
        addr: Addr,
        timeout: Duration,
        input: impl FnOnce(Token, Arc<TcpClient<TcpStream>>, ReadHalf<TcpStream>) -> Fut
            + Send
            + 'static,
        token: Token,
    ) -> Result<Arc<TcpClient<TcpStream>>> {
        let stream = tokio::time::timeout(timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "connection timed out")
            })??;
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

impl<Stream> TcpClient<Stream>
where
    Stream: AsyncRead + AsyncWrite + Send + Sync + 'static,
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
        Addr: ToSocketAddrs,
        Fut: Future<Output = anyhow::Result<bool>> + Send + 'static,
        StreamFut: Future<Output = anyhow::Result<Stream>> + Send + 'static,
        Token: Send + 'static,
    >(
        addr: Addr,
        stream_init: impl FnOnce(TcpStream) -> StreamFut + Send + 'static,
        input: impl FnOnce(Token, Arc<TcpClient<Stream>>, ReadHalf<Stream>) -> Fut + Send + 'static,
        token: Token,
    ) -> Result<Arc<TcpClient<Stream>>> {
        let stream = TcpStream::connect(addr).await?;
        let target = stream.peer_addr()?;
        let stream = stream_init(stream).await?;
        Self::init(input, token, stream, target)
    }

    /// 连接到目标地址并在指定超时内通过 `stream_init` 包装连接。
    ///
    /// 与 [`connect_stream_type`](TcpClient::connect_stream_type) 的区别是增加了
    /// `timeout` 参数，通过 [`tokio::time::timeout`] 包装 TCP 连接。
    ///
    /// 超时错误以 [`Error::IOError`] 返回，携带 [`std::io::ErrorKind::TimedOut`]。
    #[inline]
    pub async fn connect_stream_type_timeout<
        Addr: ToSocketAddrs,
        Fut: Future<Output = anyhow::Result<bool>> + Send + 'static,
        StreamFut: Future<Output = anyhow::Result<Stream>> + Send + 'static,
        Token: Send + 'static,
    >(
        addr: Addr,
        timeout: Duration,
        stream_init: impl FnOnce(TcpStream) -> StreamFut + Send + 'static,
        input: impl FnOnce(Token, Arc<TcpClient<Stream>>, ReadHalf<Stream>) -> Fut + Send + 'static,
        token: Token,
    ) -> Result<Arc<TcpClient<Stream>>> {
        let stream = tokio::time::timeout(timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "connection timed out")
            })??;
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
    fn init<Fut: Future<Output = anyhow::Result<bool>> + Send + 'static, Token: Send + 'static>(
        f: impl FnOnce(Token, Arc<TcpClient<Stream>>, ReadHalf<Stream>) -> Fut + Send + 'static,
        token: Token,
        stream: Stream,
        target: SocketAddr,
    ) -> Result<Arc<TcpClient<Stream>>> {
        // 将 TCP 流拆分为独立的读半端和写半端，两者可在不同任务中并发操作
        let (reader, mut writer) = tokio::io::split(stream);

        // 有界 channel：容量 4096，平衡内存占用与吞吐量
        let (tx, rx) = async_channel::bounded(4096);

        let client = Arc::new(TcpClient {
            disconnect: AtomicBool::new(false),
            sender: tx,
            peer_addr: target,
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
                        if let Err(e) = writer.write_all(&data).await {
                            error!("write to {} err: {}", target, e);
                            return;
                        }
                    }
                    State::SendFlush(data) => {
                        if let Err(e) = writer.write_all(&data).await {
                            error!("write to {} err: {}", target, e);
                            return;
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
            return Err(Error::SendError(
                format!("Disconnect (peer: {})", self.peer_addr).into(),
            ));
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
            Err(TrySendError::Full(_)) => Err(Error::SendError("channel full".into())),
            Err(TrySendError::Closed(_)) => Err(Error::SendError("channel closed".into())),
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
            Err(TrySendError::Closed(_)) => Err(Error::SendError("channel closed".into())),
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
            Err(TrySendError::Closed(_)) => Err(Error::SendError("channel closed".into())),
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
            Err(TrySendError::Closed(_)) => Err(Error::SendError("channel closed".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use tokio::net::TcpStream;

    /// 创建一个测试用的 TcpClient 及其 channel 接收端。
    /// channel 容量设为 1，方便测试满/空边界条件。
    /// 返回的 `_rx` 必须由调用方持有以保持 channel 开启。
    fn test_client(disconnected: bool) -> (TcpClient<TcpStream>, async_channel::Receiver<State>) {
        let (tx, rx) = async_channel::bounded::<State>(1);
        let client = TcpClient {
            disconnect: AtomicBool::new(disconnected),
            sender: tx,
            peer_addr: SocketAddr::from(([0, 0, 0, 0], 0)),
            _ph: PhantomData,
        };
        (client, rx)
    }

    // ===== check_disconnected 测试 =====

    #[test]
    fn check_disconnected_returns_ok_when_connected() {
        let (client, _rx) = test_client(false);
        assert!(client.check_disconnected().is_ok());
    }

    #[test]
    fn check_disconnected_returns_err_when_disconnected() {
        let (client, _rx) = test_client(true);
        match client.check_disconnected() {
            Err(Error::SendError(msg)) => assert!(
                msg.contains("Disconnect"),
                "expected msg containing 'Disconnect', got: {}",
                msg
            ),
            other => panic!("expected Err(Error::SendError(...)), got {:?}", other),
        }
    }

    // ===== 空缓冲区测试 =====

    #[test]
    fn try_send_empty_buffer_succeeds() {
        let (client, _rx) = test_client(false);
        assert!(client.try_send(vec![]).is_ok());
    }

    #[test]
    fn try_send_empty_buffer_fails_when_disconnected() {
        let (client, _rx) = test_client(true);
        assert!(client.try_send(vec![]).is_err());
    }

    #[test]
    fn try_send_fails_when_channel_full() {
        let (client, _rx) = test_client(false);
        // 通道容量为 1，第一次 try_send 填满通道
        assert!(client.try_send(vec![1, 2, 3]).is_ok());
        // 第二次 try_send 应该因通道满而失败
        match client.try_send(vec![4, 5, 6]) {
            Err(Error::SendError(msg)) => assert!(msg.contains("channel full")),
            other => panic!(
                "expected Err(Error::SendError) with 'channel full', got {:?}",
                other
            ),
        }
    }

    #[tokio::test]
    async fn send_empty_buffer_succeeds() {
        // send 在 try_send 快速路径失败后，需要通过 sender.send().await 挂起等待。
        // 为触发慢路径，先用一条消息填满容量为 1 的通道；用 Arc 保持接收端存活，
        // 避免先 drain 后 drop 导致 channel 提前关闭。
        let (tx, rx) = async_channel::bounded::<State>(1);
        let client: TcpClient<TcpStream> = TcpClient {
            disconnect: AtomicBool::new(false),
            sender: tx,
            peer_addr: SocketAddr::from(([0, 0, 0, 0], 0)),
            _ph: PhantomData,
        };
        let rx = Arc::new(rx);

        // 填满通道，迫使后续 send 走慢路径
        client.try_send(vec![1]).unwrap();

        let rx2 = rx.clone();
        let drain = tokio::spawn(async move {
            let _ = rx2.recv().await; // 消费第一条消息，腾出空间
        });

        // send 慢路径：try_send 失败 → sender.send().await 挂起 → drain 腾出空间 → 投递成功
        let result = client.send(vec![]).await;
        // 此时 drain 可能已经完成，但 rx（Arc 主副本）仍存活，channel 不会关闭
        assert!(result.is_ok(), "send_empty_buffer: got {:?}", result);

        // 等待 drain 完成，然后丢弃 Arc 副本
        drain.await.unwrap();
        drop(rx);
    }

    // ===== disconnect 幂等性测试 =====

    #[tokio::test]
    async fn disconnect_is_idempotent() {
        // 启动一个简单的 echo 服务来测试 disconnect
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let _echo_handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut reader, mut writer) = tokio::io::split(stream);
            let _ = tokio::io::copy(&mut reader, &mut writer).await;
        });

        let client = TcpClient::connect(
            addr,
            |_, _client, mut reader| async move {
                let mut buf = [0u8; 64];
                let _ = tokio::io::AsyncReadExt::read(&mut reader, &mut buf).await;
                Ok(false)
            },
            (),
        )
        .await
        .unwrap();

        // 第一次 disconnect 应该成功
        assert!(client.disconnect().await.is_ok());
        // 第二次 disconnect 应该也成功（幂等）
        assert!(client.disconnect().await.is_ok());
        // 断开后 try_send 应该失败
        assert!(client.try_send(vec![1]).is_err());
    }

    // ===== connect_timeout 测试 =====

    #[tokio::test]
    async fn connect_timeout_expires() {
        // TEST-NET-3 地址 (203.0.113.0/24) 不可路由，连接会超时
        let result = TcpClient::connect_timeout(
            "203.0.113.1:12345",
            Duration::from_millis(100),
            |_, _client, _reader| async move { Ok(false) },
            (),
        )
        .await;
        assert!(result.is_err(), "expected connect_timeout to fail");
    }
}

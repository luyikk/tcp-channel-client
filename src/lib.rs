pub mod error;
use async_channel::Sender;
use error::{Error, Result};
use log::*;
use std::future::Future;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadHalf};
use tokio::net::{TcpStream, ToSocketAddrs};

pub struct TcpClient<T> {
    disconnect: AtomicBool,
    sender: Sender<State>,
    _ph: PhantomData<T>,
}

impl TcpClient<TcpStream> {
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

pub enum State {
    Disconnect,
    Send(Vec<u8>),
    SendFlush(Vec<u8>),
    Flush,
}

impl<T> TcpClient<T>
where
    T: AsyncRead + AsyncWrite + Send + Sync + 'static,
{
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

    #[inline]
    fn init<F: Future<Output = anyhow::Result<bool>> + Send + 'static, A: Send + 'static>(
        f: impl FnOnce(A, Arc<TcpClient<T>>, ReadHalf<T>) -> F + Send + 'static,
        token: A,
        stream: T,
        target: SocketAddr,
    ) -> Result<Arc<TcpClient<T>>> {
        let (reader, mut sender) = tokio::io::split(stream);

        let (tx, rx) = async_channel::bounded(4096);

        let client = Arc::new(TcpClient {
            disconnect: AtomicBool::new(false),
            sender: tx,
            _ph: Default::default(),
        });
        let read_client = client.clone();
        tokio::spawn(async move {
            let disconnect_client = read_client.clone();
            let need_disconnect = f(token, read_client, reader).await.unwrap_or_else(|err| {
                error!("reader error:{}", err);
                true
            });

            if need_disconnect {
                if let Err(er) = disconnect_client.disconnect().await {
                    error!("disconnect to{} err:{}", target, er);
                } else {
                    debug!("disconnect to {}", target)
                }
            } else {
                debug!("{} reader is close", target);
            }
        });

        tokio::spawn(async move {
            loop {
                if let Ok(state) = rx.recv().await {
                    match state {
                        State::Disconnect => {
                            let _ = sender.shutdown().await;
                            return;
                        }
                        State::Send(data) => {
                            if sender.write(&data).await.is_err() {
                                return;
                            }
                        }
                        State::SendFlush(data) => {
                            if sender.write(&data).await.is_err() {
                                return;
                            }

                            if sender.flush().await.is_err() {
                                return;
                            }
                        }
                        State::Flush => {
                            if sender.flush().await.is_err() {
                                return;
                            }
                        }
                    }
                } else {
                    return;
                }
            }
        });

        Ok(client)
    }

    #[inline]
    pub async fn disconnect(&self) -> Result<()> {
        if !self.disconnect.load(Ordering::Acquire) {
            self.sender.send(State::Disconnect).await?;
            self.disconnect.store(true, Ordering::Release);
        }
        Ok(())
    }
    #[inline]
    pub async fn send(&self, buff: Vec<u8>) -> Result<()> {
        if !self.disconnect.load(Ordering::Acquire) {
            Ok(self.sender.send(State::Send(buff)).await?)
        } else {
            Err(Error::SendError("Disconnect".to_string()))
        }
    }

    #[inline]
    pub async fn send_all(&self, buff: Vec<u8>) -> Result<()> {
        if !self.disconnect.load(Ordering::Acquire) {
            Ok(self.sender.send(State::SendFlush(buff)).await?)
        } else {
            Err(Error::SendError("Disconnect".to_string()))
        }
    }

    #[inline]
    pub async fn flush(&self) -> Result<()> {
        if !self.disconnect.load(Ordering::Acquire) {
            Ok(self.sender.send(State::Flush).await?)
        } else {
            Err(Error::SendError("Disconnect".to_string()))
        }
    }
}

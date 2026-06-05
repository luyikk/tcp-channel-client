use crate::State;
use std::borrow::Cow;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    /// 发送失败（已断开连接或通道满/关闭）。
    #[error("{0}")]
    SendError(Cow<'static, str>),
    #[error(transparent)]
    IOError(#[from] std::io::Error),
    #[error(transparent)]
    General(#[from] anyhow::Error),
    #[error(transparent)]
    AsyncChannelError(#[from] async_channel::SendError<State>),
}

pub type Result<T, E = Error> = core::result::Result<T, E>;

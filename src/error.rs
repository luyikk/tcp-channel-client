use crate::State;
use std::borrow::Cow;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    /// 发送失败（已断开连接或通道满/关闭）。
    #[error("{0}")]
    SendError(Cow<'static, str>),

    /// I/O 错误（读写、连接、超时等底层流错误）。
    #[error(transparent)]
    IOError(#[from] std::io::Error),

    /// 通用错误，来自 [`anyhow`] 的任意错误类型。
    #[error(transparent)]
    General(#[from] anyhow::Error),

    /// 内部 channel 发送失败（writer 任务已退出）。
    #[error(transparent)]
    AsyncChannelError(#[from] async_channel::SendError<State>),
}

pub type Result<T, E = Error> = core::result::Result<T, E>;

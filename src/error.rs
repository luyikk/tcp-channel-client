use crate::State;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("{0}")]
    SendError(String),
    #[error(transparent)]
    IOError(#[from] std::io::Error),
    #[error(transparent)]
    Error(#[from] anyhow::Error),
    #[error(transparent)]
    AsyncChannelError(#[from] async_channel::SendError<State>),
}

pub type Result<T, E = Error> = core::result::Result<T, E>;

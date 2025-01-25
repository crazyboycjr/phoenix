use std::io;

use thiserror::Error;

use ipc::service::ShmService;
use phoenix_api::engine::SchedulingHint;
use phoenix_api::ibprovider::{cmd, dp};

use crate::{PHOENIX_CONTROL_SOCK, PHOENIX_PREFIX};

#[cfg(feature = "capi")]
pub mod verbs;

thread_local! {
    pub(crate) static DRV_CTX: Context = Context::register().expect("phoenix ibprovider register failed");
}

pub(crate) struct Context {
    service: ShmService<cmd::Command, cmd::Completion, dp::WorkRequestSlot, dp::CompletionSlot>,
}

impl Context {
    fn register() -> Result<Context, Error> {
        let service = ShmService::register(
            &*PHOENIX_PREFIX,
            &*PHOENIX_CONTROL_SOCK,
            "IbProvider".to_string(),
            SchedulingHint::default(),
            None,
        )?;
        Ok(Self { service })
    }
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("Service error: {0}")]
    Service(#[from] ipc::Error),
    #[error("Serde-json: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("IO Error {0}")]
    Io(#[from] io::Error),
    #[error("Interface error {0}: {1}")]
    Interface(&'static str, phoenix_api::Error),
    #[error("No address is resolved")]
    NoAddrResolved,
    #[error("Connect failed: {0}")]
    Connect(phoenix_api::Error),
}

impl Error {
    pub fn as_i32(&self) -> i32 {
        eprintln!("{}", self);
        match self {
            Error::Service(_) => 1,
            Error::Serde(_) => 2,
            Error::Io(_) => 3,
            Error::Interface(..) => 4,
            Error::NoAddrResolved => 5,
            Error::Connect(_) => 6,
        }
    }
}

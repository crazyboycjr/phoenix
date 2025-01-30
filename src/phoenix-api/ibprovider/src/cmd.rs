//! Control path commands.
use serde::{Deserialize, Serialize};

use phoenix_api::Handle;

type IResult<T> = Result<T, phoenix_api::Error>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    // device_name, handle of ibv_context
    GetContext(String, Handle),
    // handle of ibv_context
    AllocPd(Handle),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CompletionKind {
    GetContext,
    // returns handle of ibv_pd
    AllocPd(Handle),
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Completion(pub IResult<CompletionKind>);

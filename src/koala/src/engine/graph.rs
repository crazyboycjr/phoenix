use std::sync::mpsc::{Receiver, Sender};
use unique::Unique;

use interface::engine::EngineType;

use crate::mrpc::marshal::RpcMessage;

// pub(crate) type IQueue = Receiver<Box<dyn RpcMessage>>;
// pub(crate) type OQueue = Sender<Box<dyn RpcMessage>>;
// TODO(cjr): change to non-blocking async-friendly SomeChannel<ShmPtr<dyn RpcMessage>>,
pub(crate) type IQueue = Receiver<Unique<dyn RpcMessage>>;
pub(crate) type OQueue = Sender<Unique<dyn RpcMessage>>;

pub(crate) trait Vertex {
    fn id(&self) -> &str;
    fn engine_type(&self) -> EngineType;
    fn tx_inputs(&self) -> &Vec<IQueue>;
    fn tx_outputs(&self) -> &Vec<OQueue>;
    fn rx_inputs(&self) -> &Vec<IQueue>;
    fn rx_outputs(&self) -> &Vec<OQueue>;
}
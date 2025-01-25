use std::pin::Pin;

use anyhow::{anyhow, Context, Result};
use futures::future::BoxFuture;

use phoenix_api::ibprovider::cmd;
use phoenix_api::Handle;

use super::module::CustomerType;
use super::state::State as IbProviderState;
use super::ControlPathError;

use phoenix_common::engine::datapath::DataPathNode;
use phoenix_common::engine::future;
use phoenix_common::engine::{Decompose, Engine, EngineResult, Indicator};
use phoenix_common::envelop::ResourceDowncast;
use phoenix_common::impl_vertex_for_engine;
use phoenix_common::module::{ModuleCollection, Version};
use phoenix_common::storage::{ResourceCollection, SharedStorage};
use phoenix_common::tracing;

pub struct IbProviderEngine {
    pub(crate) customer: CustomerType,
    pub(crate) indicator: Indicator,
    pub(crate) node: DataPathNode,
    pub(crate) state: IbProviderState,
}

impl_vertex_for_engine!(IbProviderEngine, node);

impl Decompose for IbProviderEngine {
    #[inline]
    fn flush(&mut self) -> Result<usize> {
        Ok(0)
    }

    fn decompose(
        self: Box<Self>,
        _shared: &mut SharedStorage,
        _global: &mut ResourceCollection,
    ) -> (ResourceCollection, DataPathNode) {
        let engine = *self;
        let mut collections = ResourceCollection::with_capacity(2);
        tracing::trace!("dumping IbProvider engine states...");
        collections.insert("customer".to_string(), Box::new(engine.customer));
        collections.insert("state".to_string(), Box::new(engine.state));
        (collections, engine.node)
    }
}

impl IbProviderEngine {
    pub(crate) fn restore(
        mut local: ResourceCollection,
        _shared: &mut SharedStorage,
        _global: &mut ResourceCollection,
        node: DataPathNode,
        _plugged: &ModuleCollection,
        _prev_version: Version,
    ) -> Result<Self> {
        tracing::trace!("restoring IbProvider engine");
        let customer = *local
            .remove("customer")
            .unwrap()
            .downcast::<CustomerType>()
            .map_err(|x| anyhow!("fail to downcast, type_name={:?}", x.type_name()))?;
        let state = *local
            .remove("state")
            .unwrap()
            .downcast::<IbProviderState>()
            .map_err(|x| anyhow!("fail to downcast, type_name={:?}", x.type_name()))?;

        let engine = IbProviderEngine {
            customer,
            indicator: Default::default(),
            node,
            state,
        };
        Ok(engine)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Progress(usize),
    Disconnected,
}

use Status::Progress;

impl Engine for IbProviderEngine {
    fn description(self: Pin<&Self>) -> String {
        "IbProviderEngine".to_owned()
    }

    #[inline]
    fn tracker(self: Pin<&mut Self>) -> &mut Indicator {
        &mut self.get_mut().indicator
    }

    fn activate<'a>(self: Pin<&'a mut Self>) -> BoxFuture<'a, EngineResult> {
        Box::pin(async move { self.get_mut().mainloop().await })
    }
}

impl IbProviderEngine {
    async fn mainloop(&mut self) -> EngineResult {
        loop {
            let mut nwork = 0;
            match self.check_cmd()? {
                Progress(n) => nwork += n,
                Status::Disconnected => return Ok(()),
            }
            self.indicator.set_nwork(nwork);
            future::yield_now().await;
        }
    }
}

impl IbProviderEngine {
    fn check_cmd(&mut self) -> Result<Status> {
        match self.customer.try_recv_cmd() {
            Ok(req) => {
                let result = self.process_cmd(req);
                match result {
                    Ok(res) => self.customer.send_comp(cmd::Completion(Ok(res)))?,
                    Err(e) => self.customer.send_comp(cmd::Completion(Err(e.into())))?,
                }
                Ok(Progress(1))
            }
            Err(ipc::TryRecvError::Empty) => {
                // do nothing
                Ok(Progress(0))
            }
            Err(ipc::TryRecvError::Disconnected) => Ok(Status::Disconnected),
            Err(ipc::TryRecvError::Other(e)) => Err(ControlPathError::IpcTryRecv(e).into()),
        }
    }

    fn process_cmd(&mut self, req: cmd::Command) -> Result<cmd::CompletionKind> {
        use cmd::Command;
        match req {
            Command::GetContext(device_name, user_ibv_ctx_handle) => {
                tracing::debug!("GetContext, device: {}", device_name);
                let dev_list = safeverbs::devices()
                    .map_err(ControlPathError::Ibverbs)
                    .context("GetContext: safeverbs::devices()")?;
                let device = dev_list
                    .iter()
                    // if unwrap panics, it should only affect the current runtime, and the runtime
                    // should be able to recover.
                    .find(|dev| {
                        dev.name().expect("ibv_device_name failed").to_bytes()
                            == device_name.as_bytes()
                    })
                    .ok_or(ControlPathError::DeviceNotFound(device_name))?;
                let ctx = safeverbs::Context::with_device(&device)
                    .map_err(ControlPathError::Ibverbs)
                    .context("GetContext: Context::with_device()")?;
                self.state
                    .resource()
                    .ctx_table
                    .occupy_or_create_resource(user_ibv_ctx_handle, ctx);
                Ok(cmd::CompletionKind::GetContext)
            }
            Command::AllocPd(user_ibv_ctx_handle) => {
                tracing::debug!("AllocPd");
                let ctx = self.state.resource().ctx_table.get(&user_ibv_ctx_handle)?;
                let pd = ctx
                    .alloc_pd()
                    .map_err(ControlPathError::Ibverbs)
                    .context("AllocPd: ctx.alloc_pd()")?;
                let pd_handle = self
                    .state
                    .resource()
                    .pd_table
                    .insert(pd)
                    .context("insert pd failed")?;
                Ok(cmd::CompletionKind::AllocPd(Handle(pd_handle as u64)))
            }
        }
    }
}

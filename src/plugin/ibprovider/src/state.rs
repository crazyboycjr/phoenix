use std::io;
use std::sync::Arc;

use nix::unistd::Pid;

use phoenix_common::resource::{ResourceTable, ResourceSlab};
use phoenix_common::state_mgr::ProcessShared;

pub struct State {
    pub(crate) shared: Arc<Shared>,
}

impl State {
    pub fn new(shared: Arc<Shared>) -> Self {
        State { shared }
    }
}

impl State {
    #[inline]
    pub fn resource(&self) -> &Resource {
        &self.shared.resource
    }
}

pub struct Shared {
    pub pid: Pid,
    pub resource: Resource,
}

impl ProcessShared for Shared {
    type Err = io::Error;

    fn new(pid: Pid) -> io::Result<Self> {
        let shared = Shared {
            pid,
            resource: Resource::new(),
        };
        Ok(shared)
    }
}

pub struct Resource {
    pub(crate) ctx_table: ResourceTable<safeverbs::Context>,
    pub(crate) pd_table: ResourceSlab<safeverbs::ProtectionDomain>,
}

impl Resource {
    fn new() -> Self {
        Self {
            ctx_table: ResourceTable::default(),
            pd_table: ResourceSlab::default(),
        }
    }
}

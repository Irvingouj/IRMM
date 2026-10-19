//! Fake network platform: records the mesh interface the agent asked the OS for.

use std::sync::Mutex;

use async_trait::async_trait;
use irmm_agent::{MeshInterface, NetworkPlatform};

#[derive(Default)]
pub struct FakeNetwork {
    current: Mutex<Option<MeshInterface>>,
}

impl FakeNetwork {
    /// The configuration the OS would have now; `None` when no mesh interface exists.
    pub fn current(&self) -> Option<MeshInterface> {
        self.current.lock().unwrap().clone()
    }
}

#[async_trait]
impl NetworkPlatform for FakeNetwork {
    async fn apply(&self, iface: &MeshInterface) -> anyhow::Result<()> {
        *self.current.lock().unwrap() = Some(iface.clone());
        Ok(())
    }

    async fn remove(&self) -> anyhow::Result<()> {
        *self.current.lock().unwrap() = None;
        Ok(())
    }
}

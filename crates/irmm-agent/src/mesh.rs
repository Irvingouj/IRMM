//! Mesh VPN: applies the server's mesh config to the OS through a [`NetworkPlatform`].

use async_trait::async_trait;
use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use irmm_proto::MeshConfig;
use tokio::sync::watch;

/// The mesh interface as the OS should have it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshInterface {
    pub ipv4: Ipv4Net,
    pub ipv6: Ipv6Net,
    /// The client's whole mesh network, routed into the interface.
    pub routes: Vec<IpNet>,
}

impl MeshInterface {
    fn from_config(c: &MeshConfig) -> Self {
        Self {
            ipv4: c.ipv4,
            ipv6: c.ipv6,
            routes: vec![c.ipv4.trunc().into(), c.ipv6.trunc().into()],
        }
    }
}

/// OS networking the mesh needs.
#[async_trait]
pub trait NetworkPlatform: Send + Sync {
    /// Brings the mesh interface to exactly `iface`, replacing any earlier configuration.
    async fn apply(&self, iface: &MeshInterface) -> anyhow::Result<()>;
    /// Removes the mesh interface and everything `apply` set up.
    async fn remove(&self) -> anyhow::Result<()>;
}

pub struct NoNetwork;

#[async_trait]
impl NetworkPlatform for NoNetwork {
    async fn apply(&self, _iface: &MeshInterface) -> anyhow::Result<()> {
        anyhow::bail!("mesh networking is not supported on this platform yet")
    }

    async fn remove(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Applies the latest config from the server; keeps the interface across reconnects.
pub async fn run(platform: std::sync::Arc<dyn NetworkPlatform>, mut configs: watch::Receiver<Option<MeshConfig>>) {
    let mut applied: Option<MeshInterface> = None;
    while configs.changed().await.is_ok() {
        let want = configs.borrow_and_update().as_ref().map(MeshInterface::from_config);
        if want == applied {
            continue;
        }
        let res = match &want {
            Some(iface) => platform.apply(iface).await,
            None => platform.remove().await,
        };
        match res {
            Ok(()) => applied = want,
            Err(e) => tracing::warn!(error = %format!("{e:#}"), "cannot configure mesh interface"),
        }
    }
}

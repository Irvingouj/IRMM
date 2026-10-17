//! Signed self-update.

use async_trait::async_trait;
use iroh::PublicKey;
use irmm_proto::ReleaseInfo;

#[async_trait]
pub trait Updater: Send + Sync {
    /// Installs a verified binary and arranges for the new version to run.
    async fn install(&self, release: &ReleaseInfo, binary: Vec<u8>) -> anyhow::Result<()>;
}

pub struct SelfReplace;

#[async_trait]
impl Updater for SelfReplace {
    async fn install(&self, _release: &ReleaseInfo, _binary: Vec<u8>) -> anyhow::Result<()> {
        anyhow::bail!("not implemented")
    }
}

pub async fn apply(_updater: &dyn Updater, _server_url: &str, _release: &ReleaseInfo, _key: &PublicKey) -> anyhow::Result<()> {
    anyhow::bail!("not implemented")
}

use crate::Ctx;

pub async fn answer(_ctx: Ctx, _offer: String) -> anyhow::Result<String> {
    anyhow::bail!("webrtc not implemented yet")
}

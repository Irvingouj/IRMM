//! Channel streams: the first frame is a `ChannelOpen` with a server-signed ticket.

mod files;
mod rdp;
pub mod rtc;
mod shell;


use std::sync::Arc;

use anyhow::{Context as _, bail};
use async_trait::async_trait;
use irmm_proto::{ChannelKind, ChannelOpen, ChannelReply, read_frame, write_frame};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::Ctx;

pub type BoxRead = Box<dyn AsyncRead + Send + Unpin>;
pub type BoxWrite = Box<dyn AsyncWrite + Send + Unpin>;

pub trait AsyncStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> AsyncStream for T {}

/// Hands out byte streams to an RDP server (TLS security) showing one monitor of this machine.
#[async_trait]
pub trait DesktopProvider: Send + Sync {
    fn monitors(&self) -> Vec<irmm_proto::MonitorInfo>;
    async fn connect(&self, monitor: u32) -> anyhow::Result<Box<dyn AsyncStream>>;
}

pub struct NoDesktop;

#[async_trait]
impl DesktopProvider for NoDesktop {
    fn monitors(&self) -> Vec<irmm_proto::MonitorInfo> {
        Vec::new()
    }

    async fn connect(&self, _monitor: u32) -> anyhow::Result<Box<dyn AsyncStream>> {
        bail!("remote desktop is not available on this agent")
    }
}

/// Runs the RDP host inside the agent process (desktop sessions where the agent
/// itself can capture the screen, and tests).
pub struct InProcessDesktop(pub Arc<dyn irmm_desktop::Desktop>);

#[async_trait]
impl DesktopProvider for InProcessDesktop {
    fn monitors(&self) -> Vec<irmm_proto::MonitorInfo> {
        self.0
            .monitors()
            .into_iter()
            .map(|m| irmm_proto::MonitorInfo { index: m.index, name: m.name, width: m.width, height: m.height, primary: m.primary })
            .collect()
    }

    async fn connect(&self, monitor: u32) -> anyhow::Result<Box<dyn AsyncStream>> {
        let session = self.0.open(monitor)?;
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        irmm_desktop::spawn(theirs, session);
        Ok(Box::new(ours))
    }
}

pub async fn serve(ctx: Ctx, mut r: BoxRead, mut w: BoxWrite) -> anyhow::Result<()> {
    let open: ChannelOpen = read_frame(&mut r).await?.context("empty channel")?;
    let claims = match check_ticket(&ctx, &open.ticket) {
        Ok(c) => c,
        Err(e) => {
            write_frame(&mut w, &ChannelReply::Denied { reason: format!("{e:#}") }).await?;
            return Err(e);
        }
    };
    write_frame(&mut w, &ChannelReply::Accepted).await?;
    tracing::info!(kind = ?claims.kind, session = %claims.session_id, user = %claims.user_id, "channel opened");
    match claims.kind {
        ChannelKind::Rdp => rdp::serve(ctx, claims.monitor, r, w).await,
        ChannelKind::Shell => shell::serve(r, w).await,
        ChannelKind::Files => files::serve(r, w).await,
    }
}

fn check_ticket(ctx: &Ctx, ticket: &str) -> anyhow::Result<irmm_proto::TicketClaims> {
    let claims = irmm_proto::verify_ticket(&ctx.server_id, ticket)?;
    if claims.device_id != ctx.device_id {
        bail!("ticket is for another device");
    }
    if !ctx.used_tickets.lock().unwrap().insert(claims.jti) {
        bail!("ticket already used");
    }
    Ok(claims)
}

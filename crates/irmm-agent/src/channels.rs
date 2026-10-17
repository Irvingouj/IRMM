//! Channel streams: the first frame is a `ChannelOpen` with a server-signed ticket.

mod files;
mod rdp;
pub mod rtc;
mod shell;


use anyhow::{Context as _, bail};
use async_trait::async_trait;
use irmm_proto::{ChannelKind, ChannelOpen, ChannelReply, read_frame, write_frame};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::Ctx;

pub type BoxRead = Box<dyn AsyncRead + Send + Unpin>;
pub type BoxWrite = Box<dyn AsyncWrite + Send + Unpin>;

pub trait AsyncStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> AsyncStream for T {}

/// Something that can hand out a byte stream to an RDP server (TLS security) showing this machine's desktop.
#[async_trait]
pub trait DesktopProvider: Send + Sync {
    async fn connect(&self) -> anyhow::Result<Box<dyn AsyncStream>>;
}

pub struct NoDesktop;

#[async_trait]
impl DesktopProvider for NoDesktop {
    async fn connect(&self) -> anyhow::Result<Box<dyn AsyncStream>> {
        bail!("remote desktop is not available on this agent")
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
        ChannelKind::Rdp => rdp::serve(ctx, r, w).await,
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

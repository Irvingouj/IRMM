//! Browser stand-in for the WebRTC path: offerer that opens one DataChannel per channel.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, bail};
use irmm_proto::{ChannelOpen, ChannelReply, read_frame, write_frame};
use serde_json::json;
use tokio::sync::mpsc;
use webrtc::peer_connection::{PeerConnection, PeerConnectionEventHandler, RTCIceGatheringState, RTCSessionDescription};

use crate::Api;

struct Offerer {
    gathered: mpsc::Sender<()>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Offerer {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.try_send(());
        }
    }
}

/// A WebRTC session to one device; keep it alive while channels are in use.
pub struct RtcSession {
    pc: Box<dyn PeerConnection>,
}

impl RtcSession {
    /// Creates a session of `kind`, negotiates WebRTC through the server and opens its channel.
    pub async fn open(user: &Api, device: uuid::Uuid, kind: &str, monitor: u32) -> anyhow::Result<(Self, tokio::io::DuplexStream, String)> {
        let s = user.post(&format!("/api/devices/{device}/sessions"), json!({"kind": kind, "monitor": monitor})).await;
        anyhow::ensure!(s.status.is_success(), "create session: {} {}", s.status, s.body);
        let sid = s.body["session_id"].as_str().context("session id")?.to_owned();
        let ticket = s.body["ticket"].as_str().context("ticket")?.to_owned();

        let (gathered_tx, mut gathered) = mpsc::channel(1);
        let pc = irmm_agent::rtc::peer_connection(Arc::new(Offerer { gathered: gathered_tx }), &[]).await?;
        let dc = pc.create_data_channel("irmm", None).await?;
        let offer = pc.create_offer(None).await?;
        pc.set_local_description(offer).await?;
        let _ = tokio::time::timeout(Duration::from_secs(5), gathered.recv()).await;
        let local = pc.local_description().await.context("local description")?;

        let r = user.post(&format!("/api/sessions/{sid}/rtc"), json!({"sdp": local.sdp})).await;
        anyhow::ensure!(r.status.is_success(), "signal: {} {}", r.status, r.body);
        let answer = r.body["sdp"].as_str().context("answer sdp")?.to_owned();
        pc.set_remote_description(RTCSessionDescription::answer(answer)?).await?;

        let (mut stream, open) = irmm_agent::rtc::bridge(dc, false);
        tokio::time::timeout(Duration::from_secs(15), open).await.context("datachannel open timeout")??;
        write_frame(&mut stream, &ChannelOpen { ticket: ticket.clone() }).await?;
        match read_frame::<_, ChannelReply>(&mut stream).await? {
            Some(ChannelReply::Accepted) => {}
            Some(ChannelReply::Denied { reason }) => bail!("denied: {reason}"),
            None => bail!("closed"),
        }
        Ok((Self { pc: Box::new(pc) }, stream, ticket))
    }

    pub async fn close(self) {
        let _ = self.pc.close().await;
    }
}

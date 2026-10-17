//! WebRTC answerer. Each DataChannel the browser opens is one channel stream,
//! handled exactly like a gateway stream (ticket in the first frame).

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use bytes::BytesMut;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream};
use tokio::sync::mpsc;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceGatheringState, RTCIceServer, RTCPeerConnectionState, RTCSessionDescription, Registry,
};

use crate::Ctx;

/// Browsers cap SCTP messages; stay under the conservative 16 KiB limit.
const MAX_MESSAGE: usize = 16 * 1024;

/// Bridges a DataChannel to an in-memory byte stream (messages are stream chunks).
pub fn bridge(dc: Arc<dyn DataChannel>, already_open: bool) -> (DuplexStream, tokio::sync::oneshot::Receiver<()>) {
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    let (open_tx, open_rx) = tokio::sync::oneshot::channel();
    let mut open_tx = Some(open_tx);
    if already_open {
        let _ = open_tx.take().map(|t| t.send(()));
    }
    tokio::spawn(async move {
        let (mut rd, mut wr) = tokio::io::split(theirs);
        let mut buf = vec![0u8; MAX_MESSAGE];
        let mut write_open = true;
        loop {
            tokio::select! {
                ev = dc.poll() => match ev {
                    Some(DataChannelEvent::OnOpen) => { let _ = open_tx.take().map(|t| t.send(())); }
                    Some(DataChannelEvent::OnMessage(m)) => {
                        if wr.write_all(&m.data).await.is_err() { break; }
                    }
                    Some(DataChannelEvent::OnClose) | None => break,
                    Some(_) => {}
                },
                n = rd.read(&mut buf), if write_open => match n {
                    Ok(0) | Err(_) => {
                        write_open = false;
                        let _ = dc.close().await;
                    }
                    Ok(n) => {
                        if dc.send(BytesMut::from(&buf[..n])).await.is_err() { break; }
                    }
                },
            }
        }
        let _ = wr.shutdown().await;
    });
    (ours, open_rx)
}

pub fn ice_servers(urls: &[String]) -> Vec<RTCIceServer> {
    if urls.is_empty() {
        return vec![];
    }
    vec![RTCIceServer { urls: urls.to_vec(), ..Default::default() }]
}

/// Builds a data-only peer connection.
pub async fn peer_connection(
    handler: Arc<dyn PeerConnectionEventHandler>,
    ice: &[String],
) -> anyhow::Result<impl PeerConnection> {
    let config = RTCConfigurationBuilder::new().with_ice_servers(ice_servers(ice)).build();
    let pc = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_media_engine(MediaEngine::default())
        .with_interceptor_registry(Registry::new())
        .with_handler(handler)
        .with_runtime(webrtc::runtime::default_runtime().context("no async runtime")?)
        .with_udp_addrs(vec!["0.0.0.0:0".to_owned()])
        .build()
        .await?;
    Ok(pc)
}

struct Answerer {
    ctx: Ctx,
    gathered: mpsc::Sender<()>,
    ended: mpsc::Sender<()>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Answerer {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.try_send(());
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        tracing::debug!(%state, "webrtc connection state");
        if matches!(state, RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed | RTCPeerConnectionState::Disconnected) {
            let _ = self.ended.try_send(());
        }
    }

    async fn on_data_channel(&self, dc: Arc<dyn DataChannel>) {
        let ctx = self.ctx.clone();
        let (stream, _open) = bridge(dc, true);
        tokio::spawn(async move {
            let (r, w) = tokio::io::split(stream);
            if let Err(e) = super::serve(ctx, Box::new(r), Box::new(w)).await {
                tracing::debug!(error = %format!("{e:#}"), "webrtc channel ended");
            }
        });
    }
}

/// Answers a non-trickle offer; the returned SDP already contains all candidates.
pub async fn answer(ctx: Ctx, offer: String) -> anyhow::Result<String> {
    let (gathered_tx, mut gathered) = mpsc::channel(1);
    let (ended_tx, mut ended) = mpsc::channel(1);
    let ice = ctx.ice_servers.clone();
    let handler = Arc::new(Answerer { ctx, gathered: gathered_tx, ended: ended_tx });
    let pc = peer_connection(handler, &ice).await?;
    pc.set_remote_description(RTCSessionDescription::offer(offer)?).await?;
    let answer = pc.create_answer(None).await?;
    pc.set_local_description(answer).await?;
    let _ = tokio::time::timeout(Duration::from_secs(5), gathered.recv()).await;
    let local = pc.local_description().await.context("no local description")?;
    // The peer connection lives until it fails or closes (or a hard cap).
    tokio::spawn(async move {
        let _ = tokio::time::timeout(Duration::from_secs(12 * 3600), ended.recv()).await;
        let _ = pc.close().await;
    });
    Ok(local.sdp)
}

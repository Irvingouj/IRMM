//! Agent hub: accepts agent Iroh connections, handles enrollment and keeps a
//! registry of online agents for control messages and channel streams.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, bail};
use iroh::endpoint::Connection;
use irmm_proto::{AgentMsg, ChannelOpen, ChannelReply, DeviceInfo, ServerMsg, read_frame, write_frame};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::auth::hash_token;
use crate::db::{self, audit};

#[derive(Clone)]
pub struct AgentLink {
    pub conn: Connection,
    pub tx: mpsc::UnboundedSender<ServerMsg>,
    pub tenant_id: Uuid,
    link_id: u64,
}

#[derive(Default, Clone)]
pub struct Agents {
    links: Arc<Mutex<HashMap<Uuid, AgentLink>>>,
    pending_rtc: Arc<Mutex<HashMap<Uuid, oneshot::Sender<Result<String, String>>>>>,
    used_jti: Arc<Mutex<HashMap<Uuid, u64>>>,
    /// Endpoints that bootstrapped with a valid enrollment token, with expiry (unix secs).
    relay_grants: Arc<Mutex<HashMap<String, u64>>>,
    next_id: Arc<AtomicU64>,
}

impl Agents {
    pub fn is_online(&self, device: Uuid) -> bool {
        self.links.lock().unwrap().contains_key(&device)
    }

    /// Records a ticket id; false if it was already used.
    pub fn consume_jti(&self, jti: Uuid, exp: u64) -> bool {
        let mut used = self.used_jti.lock().unwrap();
        let now = irmm_proto::now_secs();
        used.retain(|_, e| *e >= now);
        used.insert(jti, exp).is_none()
    }

    /// Lets an enrolling agent use the relay before it has a device row.
    pub fn grant_relay(&self, node: String) {
        let mut g = self.relay_grants.lock().unwrap();
        let now = irmm_proto::now_secs();
        g.retain(|_, e| *e >= now);
        g.insert(node, now + 3600);
    }

    pub fn relay_granted(&self, node: &str) -> bool {
        self.relay_grants.lock().unwrap().get(node).is_some_and(|e| *e >= irmm_proto::now_secs())
    }

    pub fn get(&self, device: Uuid) -> Option<AgentLink> {
        self.links.lock().unwrap().get(&device).cloned()
    }

    pub fn send(&self, device: Uuid, msg: ServerMsg) -> bool {
        self.get(device).is_some_and(|l| l.tx.send(msg).is_ok())
    }

    pub fn online_devices(&self) -> Vec<Uuid> {
        self.links.lock().unwrap().keys().copied().collect()
    }

    /// Opens a channel stream to the agent and sends `ChannelOpen` on behalf of the client.
    pub async fn open_channel(
        &self,
        device: Uuid,
        ticket: &str,
    ) -> anyhow::Result<(iroh::endpoint::SendStream, iroh::endpoint::RecvStream)> {
        let link = self.get(device).context("device offline")?;
        let (mut send, mut recv) = link.conn.open_bi().await?;
        write_frame(&mut send, &ChannelOpen { ticket: ticket.to_owned() }).await?;
        match read_frame::<_, ChannelReply>(&mut recv).await? {
            Some(ChannelReply::Accepted) => Ok((send, recv)),
            Some(ChannelReply::Denied { reason }) => bail!("agent denied channel: {reason}"),
            None => bail!("agent closed channel"),
        }
    }

    /// Relays a WebRTC offer to the agent and waits for its answer.
    pub async fn rtc_offer(&self, device: Uuid, sdp: String) -> anyhow::Result<String> {
        let req_id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();
        self.pending_rtc.lock().unwrap().insert(req_id, tx);
        if !self.send(device, ServerMsg::RtcOffer { req_id, sdp }) {
            self.pending_rtc.lock().unwrap().remove(&req_id);
            bail!("device offline");
        }
        let res = tokio::time::timeout(Duration::from_secs(20), rx).await;
        self.pending_rtc.lock().unwrap().remove(&req_id);
        res.context("agent answer timeout")?.context("agent gone")?.map_err(|e| anyhow::anyhow!(e))
    }

    fn insert(&self, device: Uuid, link: AgentLink) {
        if let Some(old) = self.links.lock().unwrap().insert(device, link) {
            old.conn.close(0u32.into(), b"replaced");
        }
    }

    fn remove(&self, device: Uuid, link_id: u64) {
        let mut links = self.links.lock().unwrap();
        if links.get(&device).is_some_and(|l| l.link_id == link_id) {
            links.remove(&device);
        }
    }
}

pub async fn accept_loop(st: crate::State) {
    while let Some(incoming) = st.endpoint.accept().await {
        let st = st.clone();
        tokio::spawn(async move {
            let conn = match incoming.await {
                Ok(c) => c,
                Err(e) => return tracing::debug!(error = %e, "agent handshake failed"),
            };
            if let Err(e) = serve_agent(st, conn).await {
                tracing::debug!(error = %format!("{e:#}"), "agent connection ended");
            }
        });
    }
}

async fn serve_agent(st: crate::State, conn: Connection) -> anyhow::Result<()> {
    let node_id = conn.remote_id().to_string();
    let (mut send, mut recv) = conn.accept_bi().await?;
    let first: AgentMsg = read_frame(&mut recv).await?.context("agent sent nothing")?;
    let identified = match first {
        AgentMsg::Enroll { token, info } => enroll(&st, &node_id, &token, &info).await,
        AgentMsg::Hello { info } => hello(&st, &node_id, &info).await,
        _ => Err(anyhow::anyhow!("expected enroll or hello")),
    };
    let (device_id, tenant_id) = match identified {
        Ok(v) => v,
        Err(e) => {
            write_frame(&mut send, &ServerMsg::Rejected { reason: format!("{e:#}") }).await?;
            let _ = send.finish();
            tokio::time::sleep(Duration::from_millis(200)).await;
            return Ok(());
        }
    };
    write_frame(&mut send, &ServerMsg::Welcome { device_id }).await?;

    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMsg>();
    let link_id = st.agents.next_id.fetch_add(1, Ordering::Relaxed);
    st.agents.insert(
        device_id,
        AgentLink {
            conn: conn.clone(),
            tx,
            tenant_id,
            link_id,
        },
    );
    crate::releases::offer_update(&st, device_id).await;
    if let Err(e) = crate::mesh::on_connect(&st, device_id, tenant_id).await {
        tracing::warn!(error = %format!("{e:#}"), device = %device_id, "mesh config");
    }

    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write_frame(&mut send, &msg).await.is_err() {
                break;
            }
        }
    });
    let result = async {
        while let Some(msg) = read_frame::<_, AgentMsg>(&mut recv).await? {
            handle_msg(&st, device_id, tenant_id, msg).await?;
        }
        anyhow::Ok(())
    }
    .await;
    writer.abort();
    st.agents.remove(device_id, link_id);
    result
}

async fn handle_msg(st: &crate::State, device: Uuid, tenant: Uuid, msg: AgentMsg) -> anyhow::Result<()> {
    match msg {
        AgentMsg::Heartbeat { info } => update_info(st, device, tenant, &info).await?,
        AgentMsg::ScriptResult {
            run_id,
            exit_code,
            stdout,
            stderr,
            error,
        } => crate::jobs::record_result(st, tenant, device, run_id, exit_code, stdout, stderr, error).await?,
        AgentMsg::RtcAnswer { req_id, sdp } => {
            if let Some(tx) = st.agents.pending_rtc.lock().unwrap().remove(&req_id) {
                let _ = tx.send(sdp);
            }
        }
        AgentMsg::UpdateStatus { version, error } => {
            let mut tx = db::tenant_tx(&st.pool, tenant).await?;
            audit(&mut tx, tenant, None, "agent.update", Some(device.to_string()), json!({"version": version, "error": error})).await?;
            tx.commit().await?;
        }
        AgentMsg::Enroll { .. } | AgentMsg::Hello { .. } => bail!("duplicate hello"),
    }
    Ok(())
}

async fn enroll(st: &crate::State, node_id: &str, token: &str, info: &DeviceInfo) -> anyhow::Result<(Uuid, Uuid)> {
    // A device that already enrolled and lost its "enrolled" flag just says hello.
    if let Ok(found) = hello(st, node_id, info).await {
        return Ok(found);
    }
    let mut tx = db::system_tx(&st.pool).await?;
    let row: Option<(Uuid, Uuid, Uuid)> = sqlx::query_as(
        "UPDATE enroll_tokens SET uses_left = uses_left - 1
         WHERE token_hash = $1 AND uses_left > 0 AND expires_at > now()
         RETURNING id, tenant_id, site_id",
    )
    .bind(hash_token(token))
    .fetch_optional(&mut *tx)
    .await?;
    let (token_id, tenant, site) = row.context("invalid, expired or exhausted enrollment token")?;
    let device = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO devices (id, tenant_id, site_id, node_id, hostname, os, os_version, arch, agent_version, inventory)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(device)
    .bind(tenant)
    .bind(site)
    .bind(node_id)
    .bind(&info.hostname)
    .bind(&info.os)
    .bind(&info.os_version)
    .bind(&info.arch)
    .bind(&info.agent_version)
    .bind(serde_json::to_value(info)?)
    .execute(&mut *tx)
    .await?;
    audit(&mut tx, tenant, None, "device.enroll", Some(device.to_string()), json!({"hostname": info.hostname, "token_id": token_id})).await?;
    tx.commit().await?;
    Ok((device, tenant))
}

async fn hello(st: &crate::State, node_id: &str, info: &DeviceInfo) -> anyhow::Result<(Uuid, Uuid)> {
    let mut tx = db::system_tx(&st.pool).await?;
    let row: Option<(Uuid, Uuid)> = sqlx::query_as("SELECT id, tenant_id FROM devices WHERE node_id = $1")
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    let (device, tenant) = row.context("unknown device")?;
    update_info(st, device, tenant, info).await?;
    Ok((device, tenant))
}

async fn update_info(st: &crate::State, device: Uuid, tenant: Uuid, info: &DeviceInfo) -> anyhow::Result<()> {
    let mut tx = db::tenant_tx(&st.pool, tenant).await?;
    sqlx::query(
        "UPDATE devices SET hostname = $2, os = $3, os_version = $4, arch = $5, agent_version = $6, inventory = $7, last_seen = now()
         WHERE id = $1",
    )
    .bind(device)
    .bind(&info.hostname)
    .bind(&info.os)
    .bind(&info.os_version)
    .bind(&info.arch)
    .bind(&info.agent_version)
    .bind(serde_json::to_value(info)?)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

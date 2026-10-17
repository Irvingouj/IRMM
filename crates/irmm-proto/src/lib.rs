//! Wire types shared by the server, the agent and the browser.
//!
//! Every stream (Iroh bi-stream, WebSocket, WebRTC DataChannel) is a byte stream.
//! Control messages and channel headers are framed as `u32 BE length || JSON`.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use iroh::{PublicKey, SecretKey, Signature};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use uuid::Uuid;

/// ALPN of the agent ↔ server Iroh connection.
pub const AGENT_ALPN: &[u8] = b"irmm/agent/1";

const MAX_FRAME: u32 = 16 * 1024 * 1024;

pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(msg)?;
    w.write_u32(u32::try_from(body.len())?).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// Returns `None` on a clean EOF before a frame starts.
pub async fn read_frame<R, T>(r: &mut R) -> anyhow::Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let len = match r.read_u32().await {
        Ok(len) => len,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if len > MAX_FRAME {
        bail!("frame too large: {len}");
    }
    let mut body = vec![0; len as usize];
    r.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body).context("decode frame")?))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DeviceInfo {
    pub hostname: String,
    pub os: String,
    pub os_version: String,
    pub arch: String,
    pub agent_version: String,
    pub cpu_model: String,
    pub cpu_cores: u32,
    pub memory_bytes: u64,
    pub disks: Vec<DiskInfo>,
    pub ips: Vec<String>,
    pub logged_in_users: Vec<String>,
    #[serde(default)]
    pub monitors: Vec<MonitorInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MonitorInfo {
    pub index: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DiskInfo {
    pub mount: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Shell {
    Powershell,
    Cmd,
    Bash,
    Sh,
    Zsh,
    Python,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMsg {
    /// First message of a never-enrolled agent.
    Enroll { token: String, info: DeviceInfo },
    /// First message of an enrolled agent.
    Hello { info: DeviceInfo },
    Heartbeat { info: DeviceInfo },
    ScriptResult {
        run_id: Uuid,
        exit_code: Option<i32>,
        stdout: String,
        stderr: String,
        error: Option<String>,
    },
    RtcAnswer { req_id: Uuid, sdp: Result<String, String> },
    UpdateStatus { version: String, error: Option<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Welcome { device_id: Uuid },
    Rejected { reason: String },
    RunScript {
        run_id: Uuid,
        shell: Shell,
        body: String,
        timeout_secs: u64,
    },
    /// Non-trickle WebRTC offer: all ICE candidates are already in the SDP.
    RtcOffer { req_id: Uuid, sdp: String },
    Update(ReleaseInfo),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReleaseInfo {
    pub version: String,
    pub platform: String,
    pub url: String,
    /// Hex SHA-256 of the binary.
    pub sha256: String,
    /// Release-key signature over [`ReleaseInfo::signed_message`], base64url.
    pub signature: String,
}

impl ReleaseInfo {
    pub fn signed_message(version: &str, platform: &str, sha256: &str) -> Vec<u8> {
        format!("irmm-release\n{version}\n{platform}\n{sha256}").into_bytes()
    }

    pub fn verify(&self, release_key: &PublicKey) -> anyhow::Result<()> {
        let sig = decode_sig(&self.signature)?;
        release_key
            .verify(&Self::signed_message(&self.version, &self.platform, &self.sha256), &sig)
            .context("bad release signature")
    }
}

pub fn sign_release(key: &SecretKey, version: &str, platform: &str, sha256: &str) -> String {
    B64.encode(key.sign(&ReleaseInfo::signed_message(version, platform, sha256)).to_bytes())
}

/// Platform string used for releases, e.g. `windows-x86_64`.
pub fn current_platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Rdp,
    Shell,
    Files,
}

/// Claims of a session ticket. Signed by the server's Iroh secret key, so an agent
/// verifies it with the server `EndpointId` pinned at enrollment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TicketClaims {
    pub jti: Uuid,
    pub session_id: Uuid,
    pub tenant_id: Uuid,
    pub device_id: Uuid,
    pub user_id: Uuid,
    pub kind: ChannelKind,
    /// Monitor to show for `Rdp` channels.
    #[serde(default)]
    pub monitor: u32,
    /// Unix seconds.
    pub exp: u64,
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).expect("clock after epoch").as_secs()
}

pub fn sign_ticket(key: &SecretKey, claims: &TicketClaims) -> String {
    let payload = serde_json::to_vec(claims).expect("claims serialize");
    let sig = key.sign(&payload);
    format!("{}.{}", B64.encode(&payload), B64.encode(sig.to_bytes()))
}

/// Checks signature and expiry. Replay and device checks are the caller's job.
pub fn verify_ticket(key: &PublicKey, ticket: &str) -> anyhow::Result<TicketClaims> {
    let (payload, sig) = ticket.split_once('.').context("malformed ticket")?;
    let payload = B64.decode(payload).context("ticket payload")?;
    key.verify(&payload, &decode_sig(sig)?).context("bad ticket signature")?;
    let claims: TicketClaims = serde_json::from_slice(&payload)?;
    if claims.exp < now_secs() {
        bail!("ticket expired");
    }
    Ok(claims)
}

fn decode_sig(s: &str) -> anyhow::Result<Signature> {
    let bytes: [u8; 64] = B64
        .decode(s)
        .context("signature encoding")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("signature length"))?;
    Ok(Signature::from_bytes(&bytes))
}

/// First frame on every channel stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelOpen {
    pub ticket: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChannelReply {
    Accepted,
    Denied { reason: String },
}

/// Frames of a `Shell` channel (after `ChannelOpen`), both directions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShellFrame {
    /// Client → agent: start a PTY.
    Start { shell: Option<Shell>, cols: u16, rows: u16 },
    /// Base64 bytes; client → agent is keyboard input, agent → client is PTY output.
    Data { data: String },
    Resize { cols: u16, rows: u16 },
    Exit { code: Option<i32> },
}

impl ShellFrame {
    pub fn data(bytes: &[u8]) -> Self {
        Self::Data { data: B64.encode(bytes) }
    }

    pub fn decode_data(data: &str) -> anyhow::Result<Vec<u8>> {
        Ok(B64.decode(data)?)
    }
}

/// Request frame of a `Files` channel (after `ChannelOpen`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FilesRequest {
    List { path: String },
    /// Agent replies `FilesReply::Download`, then raw bytes until EOF.
    Download { path: String },
    /// Client sends raw bytes after this frame until EOF; agent replies `Done`.
    Upload { path: String, size: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FilesReply {
    List { entries: Vec<FileEntry> },
    Download { size: u64 },
    Done,
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<u64>,
}

/// Returned by `GET /api/agent/bootstrap`; pinned by the agent at enrollment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bootstrap {
    pub server_id: String,
    pub direct_addrs: Vec<String>,
    pub relay_url: Option<String>,
    pub release_key: String,
    /// STUN/TURN URLs for WebRTC.
    #[serde(default)]
    pub ice_servers: Vec<String>,
}

/// QUIC settings shared by agent and server: keep-alives plus a short idle timeout,
/// so a vanished peer (crash, power loss, network cut) is noticed within seconds.
pub fn quic_transport() -> iroh::endpoint::QuicTransportConfig {
    iroh::endpoint::QuicTransportConfig::builder()
        .keep_alive_interval(std::time::Duration::from_secs(4))
        .max_idle_timeout(Some(std::time::Duration::from_secs(15).try_into().expect("valid idle timeout")))
        .build()
}

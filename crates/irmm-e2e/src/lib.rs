//! Test kit: real Postgres, real server, real agents, talking over real sockets.

pub mod rdp;
pub mod rtc;

use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, bail};
use irmm_agent::{AgentHandle, AgentState};
use irmm_server::ServerHandle;
use reqwest::StatusCode;
use serde_json::{Value, json};

pub use serde_json;

/// Admin URL of a Postgres the tests may create databases in.
fn admin_db_url() -> String {
    std::env::var("IRMM_TEST_PG").unwrap_or_else(|_| "postgres://postgres:irmm@127.0.0.1:55432".into())
}

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
        .with_test_writer()
        .try_init();
}

pub struct TestEnv {
    pub server: ServerHandle,
    pub release_key: iroh::SecretKey,
    pub db_url: String,
    _dirs: Vec<tempdir::Dir>,
}

pub mod tempdir {
    use std::path::{Path, PathBuf};

    /// Directory removed on drop.
    pub struct Dir(PathBuf);

    impl Dir {
        pub fn new() -> Self {
            let p = std::env::temp_dir().join(format!("irmm-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&p).expect("create temp dir");
            Self(p)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }

        pub fn to_path_buf(&self) -> PathBuf {
            self.0.clone()
        }
    }

    impl Default for Dir {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

impl TestEnv {
    pub async fn start() -> Self {
        init_tracing();
        let db_name = format!("irmm_test_{}", uuid::Uuid::new_v4().simple());
        let admin = admin_db_url();
        let mut conn = <sqlx::PgConnection as sqlx::Connection>::connect(&format!("{admin}/postgres"))
            .await
            .expect("connect to test postgres (start it with: docker start irmm-test-pg)");
        sqlx::Executor::execute(&mut conn, format!("CREATE DATABASE {db_name}").as_str())
            .await
            .expect("create test db");
        let db_url = format!("{admin}/{db_name}");
        let release_key = iroh::SecretKey::generate();
        let files = tempdir::Dir::new();
        let server = irmm_server::start(irmm_server::Config {
            database_url: db_url.clone(),
            http_addr: "127.0.0.1:0".parse().unwrap(),
            iroh_addr: "127.0.0.1:0".parse().unwrap(),
            public_url: None,
            secret_key: iroh::SecretKey::generate(),
            relay_url: None,
            release_key: release_key.public(),
            data_dir: files.to_path_buf(),
            static_dir: None,
            ice_servers: vec![],
            operator_token: Some(OPERATOR_TOKEN.into()),
        })
        .await
        .expect("start server");
        Self {
            server,
            release_key,
            db_url,
            _dirs: vec![files],
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.server.http_addr)
    }

    /// Client authenticated with the operator token (release publishing).
    pub fn operator(&self) -> Api {
        Api::new(self.url(), Some(OPERATOR_TOKEN.into()))
    }

    pub fn anon(&self) -> Api {
        Api::new(self.url(), None)
    }

    /// Creates a new MSP tenant and returns its owner, already past TOTP setup.
    pub async fn signup(&self, tenant: &str, email: &str) -> Api {
        let anon = self.anon();
        anon.post("/api/signup", json!({"tenant_name": tenant, "email": email, "password": PASSWORD}))
            .await
            .expect_ok();
        anon.login(email, PASSWORD).await.expect("login")
    }

    pub fn agent_config(&self, state_dir: PathBuf, token: Option<&str>) -> irmm_agent::Config {
        irmm_agent::Config {
            server_url: self.url(),
            state_dir,
            enroll_token: token.map(str::to_owned),
            ..irmm_agent::Config::for_tests()
        }
    }

    pub async fn start_agent(&self, state_dir: PathBuf, token: Option<&str>) -> AgentHandle {
        irmm_agent::start(self.agent_config(state_dir, token)).await.expect("start agent")
    }
}

pub const OPERATOR_TOKEN: &str = "test-operator-token";

pub const PASSWORD: &str = "correct horse battery staple";

/// Polls `f` until it returns `Some`, failing after 20 s.
pub async fn eventually<T, F, Fut>(what: &str, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for: {what}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub async fn wait_agent_online(agent: &AgentHandle) -> uuid::Uuid {
    eventually("agent online", || async {
        match agent.state() {
            AgentState::Online { device_id } => Some(device_id),
            _ => None,
        }
    })
    .await
}

#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    pub base: String,
    pub token: Option<String>,
    /// TOTP secret (base32) of the logged-in user, when known.
    pub totp_secret: Option<String>,
}

#[derive(Debug)]
pub struct Resp {
    pub status: StatusCode,
    pub body: Value,
}

impl Resp {
    #[track_caller]
    pub fn expect_ok(self) -> Value {
        assert!(self.status.is_success(), "expected success, got {} {}", self.status, self.body);
        self.body
    }

    #[track_caller]
    pub fn expect_status(self, status: u16) -> Value {
        assert_eq!(self.status.as_u16(), status, "unexpected status, body: {}", self.body);
        self.body
    }
}

pub fn totp_now(secret_b32: &str) -> String {
    let secret = totp_rs::Secret::Encoded(secret_b32.to_owned()).to_bytes().expect("totp secret");
    totp_rs::TOTP::new_unchecked(totp_rs::Algorithm::SHA1, 6, 1, 30, secret, None, String::new())
        .generate_current()
        .expect("totp code")
}

impl Api {
    pub fn new(base: String, token: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base,
            token,
            totp_secret: None,
        }
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Resp {
        let req = match &self.token {
            Some(t) => req.bearer_auth(t),
            None => req,
        };
        let res = req.send().await.expect("http request");
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Resp { status, body }
    }

    pub async fn get(&self, path: &str) -> Resp {
        self.send(self.http.get(format!("{}{path}", self.base))).await
    }

    pub async fn get_bytes(&self, path: &str) -> (StatusCode, Vec<u8>) {
        let mut req = self.http.get(format!("{}{path}", self.base));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let res = req.send().await.expect("http request");
        (res.status(), res.bytes().await.expect("body").to_vec())
    }

    pub async fn post(&self, path: &str, body: Value) -> Resp {
        self.send(self.http.post(format!("{}{path}", self.base)).json(&body)).await
    }

    pub async fn post_bytes(&self, path: &str, body: Vec<u8>) -> Resp {
        self.send(self.http.post(format!("{}{path}", self.base)).body(body)).await
    }

    pub async fn patch(&self, path: &str, body: Value) -> Resp {
        self.send(self.http.patch(format!("{}{path}", self.base)).json(&body)).await
    }

    pub async fn put(&self, path: &str, body: Value) -> Resp {
        self.send(self.http.put(format!("{}{path}", self.base)).json(&body)).await
    }

    pub async fn delete(&self, path: &str) -> Resp {
        self.send(self.http.delete(format!("{}{path}", self.base))).await
    }

    /// Password + TOTP login. Enrolls TOTP on first login (it is mandatory).
    pub async fn login(&self, email: &str, password: &str) -> anyhow::Result<Api> {
        self.login_with(email, password, None).await
    }

    pub async fn login_with(&self, email: &str, password: &str, totp_secret: Option<&str>) -> anyhow::Result<Api> {
        let r = self.post("/api/auth/login", json!({"email": email, "password": password})).await;
        if !r.status.is_success() {
            bail!("login failed: {} {}", r.status, r.body);
        }
        let step = r.body["step"].as_str().context("step")?.to_owned();
        let pending = r.body["pending_token"].as_str().context("pending_token")?.to_owned();
        let secret = match step.as_str() {
            "totp_setup" => r.body["totp_secret"].as_str().context("totp_secret")?.to_owned(),
            "totp" => totp_secret.context("TOTP secret required")?.to_owned(),
            other => bail!("unexpected step {other}"),
        };
        let r = self
            .post("/api/auth/totp", json!({"pending_token": pending, "code": totp_now(&secret)}))
            .await;
        if !r.status.is_success() {
            bail!("totp failed: {} {}", r.status, r.body);
        }
        let mut api = Api::new(self.base.clone(), Some(r.body["token"].as_str().context("token")?.to_owned()));
        api.totp_secret = Some(secret);
        Ok(api)
    }
}

/// Creates client → site → enrollment token, returns `(client_id, site_id, token_response)`.
pub async fn make_site(admin: &Api, client: &str, site: &str) -> (String, String, Value) {
    let c = admin.post("/api/clients", json!({"name": client})).await.expect_ok();
    let cid = c["id"].as_str().unwrap().to_owned();
    let s = admin.post(&format!("/api/clients/{cid}/sites"), json!({"name": site})).await.expect_ok();
    let sid = s["id"].as_str().unwrap().to_owned();
    let t = admin
        .post(&format!("/api/sites/{sid}/enroll-tokens"), json!({"max_uses": 5, "ttl_hours": 24}))
        .await
        .expect_ok();
    (cid, sid, t)
}

/// Enrolls one agent in a fresh site and waits until it is online.
pub async fn enrolled_agent(env: &TestEnv, admin: &Api) -> (AgentHandle, uuid::Uuid, tempdir::Dir) {
    let (_, _, tok) = make_site(admin, "Client", "Site").await;
    let dir = tempdir::Dir::new();
    let agent = env.start_agent(dir.to_path_buf(), tok["token"].as_str()).await;
    let id = wait_agent_online(&agent).await;
    (agent, id, dir)
}

/// Native shell for scripts on the machine running the tests.
pub fn native_shell() -> &'static str {
    if cfg!(windows) { "powershell" } else { "sh" }
}

/// Opens a WebSocket and exposes it as a byte stream (binary messages are stream chunks).
pub async fn ws_stream(url: &str) -> anyhow::Result<tokio::io::DuplexStream> {
    use futures::{SinkExt as _, StreamExt as _};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio_tungstenite::tungstenite::Message;

    let (ws, _) = tokio_tungstenite::connect_async(url).await?;
    let (mut sink, mut stream) = ws.split();
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    let (mut rd, mut wr) = tokio::io::split(theirs);
    tokio::spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
                Ok(n) => {
                    if sink.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            match msg {
                Message::Binary(b) => {
                    if wr.write_all(&b).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
        let _ = wr.shutdown().await;
    });
    Ok(ours)
}

/// Creates a session and opens its channel through the WSS gateway.
pub async fn gateway_channel(env: &TestEnv, user: &Api, device: uuid::Uuid, kind: &str) -> anyhow::Result<tokio::io::DuplexStream> {
    let s = user.post(&format!("/api/devices/{device}/sessions"), json!({"kind": kind})).await;
    anyhow::ensure!(s.status.is_success(), "create session: {} {}", s.status, s.body);
    let ticket = s.body["ticket"].as_str().context("ticket")?;
    ws_stream(&gateway_url(env, device, ticket)).await
}

pub fn gateway_url(env: &TestEnv, device: uuid::Uuid, ticket: &str) -> String {
    format!("ws://{}/api/gw/{device}?ticket={ticket}", env.server.http_addr)
}

/// Enrolls an agent whose desktop is the given fake.
pub async fn enrolled_agent_with_desktop(
    env: &TestEnv,
    admin: &Api,
    desktop: rdp::FakeDesktop,
) -> (AgentHandle, uuid::Uuid, tempdir::Dir) {
    let (_, _, tok) = make_site(admin, "Client", "Site").await;
    let dir = tempdir::Dir::new();
    let mut cfg = env.agent_config(dir.to_path_buf(), tok["token"].as_str());
    cfg.desktop = std::sync::Arc::new(irmm_agent::InProcessDesktop(std::sync::Arc::new(desktop)));
    let agent = irmm_agent::start(cfg).await.expect("start agent");
    let id = wait_agent_online(&agent).await;
    (agent, id, dir)
}

/// Path to the real `irmm-agent` executable, built on first use.
pub fn agent_binary() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let status = std::process::Command::new(env!("CARGO"))
                .args(["build", "-q", "-p", "irmm-agent", "--bin", "irmm-agent"])
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .status()
                .expect("run cargo build");
            assert!(status.success(), "building irmm-agent failed");
            // target/<profile>/deps/<test> -> target/<profile>/irmm-agent
            let profile_dir = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().to_path_buf();
            profile_dir.join(format!("irmm-agent{}", std::env::consts::EXE_SUFFIX))
        })
        .clone()
}

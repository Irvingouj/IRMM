//! A running IRMM for console (browser) tests and local development: a real server
//! serving `web/dist`, plus a control endpoint that starts real agents with a fake desktop.
//!
//! Prints one JSON line `{"url": ..., "control": ...}` once ready.
//!   POST {control}/agents       {"token": "..."}  → {"device_id": ...}
//!   GET  {control}/desktop/events                  → input events the fake desktop received
//!   POST {control}/desktop/color {"rgb": [r,g,b]}

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use irmm_e2e::rdp::FakeDesktop;
use irmm_e2e::{TestEnv, tempdir, wait_agent_online};
use serde_json::{Value, json};

struct Ctl {
    env: TestEnv,
    desktop: FakeDesktop,
    agents: Mutex<Vec<(irmm_agent::AgentHandle, tempdir::Dir)>>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let web = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/dist");
    let env = TestEnv::start_with_console(Some(web.canonicalize()?)).await;
    let desktop = FakeDesktop::default();
    desktop.set_rgb(0x20, 0x90, 0xf0);
    let url = env.url();
    let ctl = Arc::new(Ctl { env, desktop, agents: Mutex::new(Vec::new()) });
    let app = Router::new()
        .route("/agents", post(start_agent))
        .route("/desktop/events", get(events))
        .route("/desktop/color", post(color))
        .with_state(ctl);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    println!("{}", json!({"url": url, "control": format!("http://{}", listener.local_addr()?)}));
    axum::serve(listener, app).await?;
    Ok(())
}

async fn start_agent(State(ctl): State<Arc<Ctl>>, Json(req): Json<Value>) -> Json<Value> {
    let dir = tempdir::Dir::new();
    let mut cfg = ctl.env.agent_config(dir.to_path_buf(), req["token"].as_str());
    cfg.desktop = Arc::new(irmm_agent::InProcessDesktop(Arc::new(ctl.desktop.clone())));
    let agent = irmm_agent::start(cfg).await.expect("start agent");
    let id = wait_agent_online(&agent).await;
    ctl.agents.lock().unwrap().push((agent, dir));
    Json(json!({"device_id": id}))
}

async fn events(State(ctl): State<Arc<Ctl>>) -> Json<Value> {
    Json(json!(ctl.desktop.events().iter().map(|e| format!("{e:?}")).collect::<Vec<_>>()))
}

async fn color(State(ctl): State<Arc<Ctl>>, Json(req): Json<Value>) -> Json<Value> {
    let c: Vec<u8> = serde_json::from_value(req["rgb"].clone()).unwrap_or_default();
    if let [r, g, b] = c[..] {
        ctl.desktop.set_rgb(r, g, b);
    }
    Json(json!({}))
}

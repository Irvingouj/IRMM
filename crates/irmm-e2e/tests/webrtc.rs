//! The P0 transport: browser ↔ agent over a WebRTC DataChannel, signaled via the server.

use std::time::Duration;

use irmm_e2e::rdp::{FakeDesktop, RdpClient};
use irmm_e2e::rtc::RtcSession;
use irmm_e2e::*;
use irmm_proto::{ShellFrame, read_frame, write_frame};

#[tokio::test]
async fn remote_desktop_over_webrtc() {
    let env = TestEnv::start().await;
    let admin = env.signup("MSP", "o@m.test").await;
    let desktop = FakeDesktop::default();
    desktop.set_rgb(0x30, 0xc0, 0x60);
    let (_agent, dev, _dir) = enrolled_agent_with_desktop(&env, &admin, desktop.clone()).await;

    let (session, stream, ticket) = RtcSession::open(&admin, dev, "rdp", 0).await.expect("webrtc session");
    let mut rdp = RdpClient::connect(stream, &dev.to_string(), &ticket).await.expect("rdp over datachannel");
    assert_eq!(rdp.size(), (640, 480));
    rdp.pump_until("frame over webrtc", |c| {
        let p = c.pixel(320, 240);
        p[..3].iter().zip([0x30u8, 0xc0, 0x60]).all(|(a, b)| a.abs_diff(b) <= 12)
    })
    .await
    .unwrap();
    session.close().await;
}

#[tokio::test]
async fn shell_over_webrtc() {
    let env = TestEnv::start().await;
    let admin = env.signup("MSP", "o@m.test").await;
    let (_agent, dev, _dir) = enrolled_agent(&env, &admin).await;
    let (session, mut s, _) = RtcSession::open(&admin, dev, "shell", 0).await.expect("webrtc session");
    write_frame(&mut s, &ShellFrame::Start { shell: None, cols: 100, rows: 30 }).await.unwrap();
    let cmd = if cfg!(windows) { "Write-Output (\"rtc-\" + (6*7))\r\n" } else { "echo rtc-$((6*7))\n" };
    write_frame(&mut s, &ShellFrame::data(cmd.as_bytes())).await.unwrap();
    let mut out = String::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        while !out.contains("rtc-42") {
            if let Some(ShellFrame::Data { data }) = read_frame::<_, ShellFrame>(&mut s).await.unwrap() {
                out.push_str(&String::from_utf8_lossy(&ShellFrame::decode_data(&data).unwrap()));
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no output; got {out}"));
    session.close().await;
}

#[tokio::test]
async fn signaling_is_scoped_to_the_session_owner() {
    let env = TestEnv::start().await;
    let a = env.signup("A", "a@a.test").await;
    let b = env.signup("B", "b@b.test").await;
    let (_agent, dev, _dir) = enrolled_agent(&env, &a).await;
    let s = a.post(&format!("/api/devices/{dev}/sessions"), serde_json::json!({"kind": "shell"})).await.expect_ok();
    let sid = s["session_id"].as_str().unwrap();
    b.post(&format!("/api/sessions/{sid}/rtc"), serde_json::json!({"sdp": "v=0"})).await.expect_status(404);
}

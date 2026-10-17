use std::time::Duration;

use irmm_e2e::*;
use irmm_proto::{ShellFrame, read_frame, write_frame};
use serde_json::json;

async fn read_until_contains(s: &mut tokio::io::DuplexStream, needle: &str) -> String {
    let mut out = String::new();
    let res = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match read_frame::<_, ShellFrame>(s).await.unwrap() {
                Some(ShellFrame::Data { data }) => {
                    out.push_str(&String::from_utf8_lossy(&ShellFrame::decode_data(&data).unwrap()));
                    if out.contains(needle) {
                        return;
                    }
                }
                Some(ShellFrame::Exit { code }) => panic!("shell exited ({code:?}) before {needle:?}; output: {out}"),
                Some(_) => {}
                None => panic!("stream closed before {needle:?}; output: {out}"),
            }
        }
    })
    .await;
    assert!(res.is_ok(), "timed out waiting for {needle:?}; output so far: {out}");
    out
}

#[tokio::test]
async fn interactive_shell_through_the_gateway() {
    let env = TestEnv::start().await;
    let admin = env.signup("MSP", "o@m.test").await;
    let (_agent, dev, _dir) = enrolled_agent(&env, &admin).await;

    let mut s = gateway_channel(&env, &admin, dev, "shell").await.unwrap();
    write_frame(&mut s, &ShellFrame::Start { shell: None, cols: 120, rows: 40 }).await.unwrap();
    let cmd = if cfg!(windows) { "Write-Output (\"irmm-\" + (40+2))\r\n" } else { "echo irmm-$((40+2))\n" };
    write_frame(&mut s, &ShellFrame::data(cmd.as_bytes())).await.unwrap();
    read_until_contains(&mut s, "irmm-42").await;

    write_frame(&mut s, &ShellFrame::Resize { cols: 80, rows: 24 }).await.unwrap();
    write_frame(&mut s, &ShellFrame::data(b"exit\r\n")).await.unwrap();
    let exited = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match read_frame::<_, ShellFrame>(&mut s).await.unwrap() {
                Some(ShellFrame::Exit { .. }) | None => return,
                _ => {}
            }
        }
    })
    .await;
    assert!(exited.is_ok(), "shell exits");
}

#[tokio::test]
async fn tickets_are_single_use_and_device_bound() {
    let env = TestEnv::start().await;
    let admin = env.signup("MSP", "o@m.test").await;
    let (_a1, d1, _x1) = enrolled_agent(&env, &admin).await;
    let (_a2, d2, _x2) = enrolled_agent(&env, &admin).await;

    let s = admin.post(&format!("/api/devices/{d1}/sessions"), json!({"kind": "shell"})).await.expect_ok();
    let ticket = s["ticket"].as_str().unwrap();

    // A ticket for d1 cannot be used for d2.
    assert!(ws_stream(&gateway_url(&env, d2, ticket)).await.is_err(), "ticket bound to device");
    // First use works, replay does not.
    let _ok = ws_stream(&gateway_url(&env, d1, ticket)).await.expect("first use");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(ws_stream(&gateway_url(&env, d1, ticket)).await.is_err(), "ticket replay rejected");
    // Garbage is rejected.
    assert!(ws_stream(&gateway_url(&env, d1, "nope.nope")).await.is_err());
}

#[tokio::test]
async fn file_upload_list_download() {
    let env = TestEnv::start().await;
    let admin = env.signup("MSP", "o@m.test").await;
    let (_agent, dev, _dir) = enrolled_agent(&env, &admin).await;
    let remote = tempdir::Dir::new();
    let path = remote.path().join("hello.bin");
    let path_q = urlencode(&path.display().to_string());
    let dir_q = urlencode(&remote.path().display().to_string());
    let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();

    admin.post_bytes(&format!("/api/devices/{dev}/files/upload?path={path_q}"), payload.clone()).await.expect_ok();
    assert_eq!(std::fs::read(&path).unwrap(), payload);

    let list = admin.get(&format!("/api/devices/{dev}/files?path={dir_q}")).await.expect_ok();
    let e = list.as_array().unwrap().iter().find(|e| e["name"] == "hello.bin").expect("listed");
    assert_eq!(e["size"], payload.len());
    assert_eq!(e["is_dir"], false);

    let (status, bytes) = admin.get_bytes(&format!("/api/devices/{dev}/files/download?path={path_q}")).await;
    assert!(status.is_success());
    assert_eq!(bytes, payload);

    let missing = urlencode(&remote.path().join("missing").display().to_string());
    let r = admin.get(&format!("/api/devices/{dev}/files/download?path={missing}")).await;
    assert_eq!(r.status.as_u16(), 400, "{}", r.body);

    let audit = admin.get("/api/audit").await.expect_ok();
    let actions: Vec<_> = audit.as_array().unwrap().iter().map(|e| e["action"].as_str().unwrap().to_owned()).collect();
    assert!(actions.contains(&"file.upload".to_owned()) && actions.contains(&"file.download".to_owned()));

    // Viewers cannot touch files.
    admin.post("/api/users", json!({"email": "v@m.test", "password": PASSWORD, "role": "viewer"})).await.expect_ok();
    let viewer = env.anon().login("v@m.test", PASSWORD).await.unwrap();
    viewer.get(&format!("/api/devices/{dev}/files?path={dir_q}")).await.expect_status(403);
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

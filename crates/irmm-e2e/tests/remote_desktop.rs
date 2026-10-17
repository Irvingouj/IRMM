use ironrdp::input::{MouseButton, MousePosition, Operation, Scancode};
use irmm_desktop::{InputEvent, MouseButton as Btn};
use irmm_e2e::rdp::{FakeDesktop, RdpClient};
use irmm_e2e::*;
use serde_json::json;

/// RemoteFX is lossy; compare colors with a tolerance.
fn close(px: [u8; 4], rgb: [u8; 3]) -> bool {
    px[..3].iter().zip(rgb).all(|(a, b)| a.abs_diff(b) <= 12)
}

async fn rdp_session(env: &TestEnv, admin: &Api, dev: uuid::Uuid, monitor: u32) -> anyhow::Result<RdpClient<tokio::io::DuplexStream>> {
    let s = admin.post(&format!("/api/devices/{dev}/sessions"), json!({"kind": "rdp", "monitor": monitor})).await.expect_ok();
    let ticket = s["ticket"].as_str().unwrap();
    let stream = ws_stream(&gateway_url(env, dev, ticket)).await?;
    RdpClient::connect(stream, &dev.to_string(), ticket).await
}

#[tokio::test]
async fn view_and_control_the_desktop_through_the_gateway() {
    let env = TestEnv::start().await;
    let admin = env.signup("MSP", "o@m.test").await;
    let desktop = FakeDesktop::default();
    desktop.set_rgb(0x20, 0x90, 0xf0);
    let (_agent, dev, _dir) = enrolled_agent_with_desktop(&env, &admin, desktop.clone()).await;

    // Monitors show up in the inventory.
    let d = admin.get(&format!("/api/devices/{dev}")).await.expect_ok();
    assert_eq!(d["inventory"]["monitors"].as_array().unwrap().len(), 2);

    let mut rdp = rdp_session(&env, &admin, dev, 0).await.expect("rdp connect");
    assert_eq!(rdp.size(), (640, 480));
    rdp.pump_until("first frame", |c| close(c.pixel(10, 10), [0x20, 0x90, 0xf0])).await.unwrap();

    // Screen changes are streamed.
    desktop.set_rgb(0xff, 0x00, 0x00);
    rdp.pump_until("updated frame", |c| close(c.pixel(600, 400), [0xff, 0x00, 0x00])).await.unwrap();

    // Mouse and keyboard reach the input sink.
    rdp.send([
        Operation::MouseMove(MousePosition { x: 100, y: 50 }),
        Operation::MouseButtonPressed(MouseButton::Left),
        Operation::MouseButtonReleased(MouseButton::Left),
        Operation::KeyPressed(Scancode::from_u8(false, 0x1E)),
        Operation::KeyReleased(Scancode::from_u8(false, 0x1E)),
    ])
    .await
    .unwrap();
    // Ctrl+Alt+Del becomes a secure attention request, not a Del key press.
    rdp.send([
        Operation::KeyPressed(Scancode::from_u8(false, 0x1D)),
        Operation::KeyPressed(Scancode::from_u8(false, 0x38)),
        Operation::KeyPressed(Scancode::from_u8(true, 0x53)),
        Operation::KeyReleased(Scancode::from_u8(true, 0x53)),
        Operation::KeyReleased(Scancode::from_u8(false, 0x38)),
        Operation::KeyReleased(Scancode::from_u8(false, 0x1D)),
    ])
    .await
    .unwrap();
    let events = eventually("input delivered", || async {
        let ev = desktop.events();
        ev.contains(&InputEvent::SecureAttention).then_some(ev)
    })
    .await;
    assert!(events.contains(&InputEvent::MouseMove { x: 100, y: 50 }), "{events:?}");
    assert!(events.contains(&InputEvent::Button { button: Btn::Left, down: true }));
    assert!(events.contains(&InputEvent::Button { button: Btn::Left, down: false }));
    assert!(events.contains(&InputEvent::Key { code: 0x1E, extended: false, down: true }));
    assert!(!events.iter().any(|e| matches!(e, InputEvent::Key { code: 0x53, down: true, .. })), "{events:?}");
}

#[tokio::test]
async fn switch_monitor() {
    let env = TestEnv::start().await;
    let admin = env.signup("MSP", "o@m.test").await;
    let desktop = FakeDesktop::default();
    desktop.set_rgb(0x40, 0xa0, 0x30);
    let (_agent, dev, _dir) = enrolled_agent_with_desktop(&env, &admin, desktop).await;
    let mut rdp = rdp_session(&env, &admin, dev, 1).await.expect("rdp connect");
    assert_eq!(rdp.size(), (320, 240));
    rdp.pump_until("frame", |c| close(c.pixel(5, 5), [0x40, 0xa0, 0x30])).await.unwrap();
}

#[tokio::test]
async fn desktop_unavailable_is_reported() {
    let env = TestEnv::start().await;
    let admin = env.signup("MSP", "o@m.test").await;
    let (_agent, dev, _dir) = enrolled_agent(&env, &admin).await;
    let err = rdp_session(&env, &admin, dev, 0).await.err().expect("no desktop → error");
    assert!(format!("{err:#}").contains("RDCleanPath"), "{err:#}");
}

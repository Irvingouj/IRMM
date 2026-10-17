//! The real Linux desktop backend against a real X server (Xvfb), driven by an RDP
//! client through the gateway. Requires `Xvfb` on the PATH.
#![cfg(target_os = "linux")]

use std::process::Stdio;

use ironrdp::input::{MouseButton, MousePosition, Operation, Scancode};
use irmm_e2e::rdp::RdpClient;
use irmm_e2e::*;
use serde_json::json;
use x11rb::connection::Connection as _;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, EventMask, InputFocus, WindowClass};

fn close(px: [u8; 4], rgb: [u8; 3]) -> bool {
    px[..3].iter().zip(rgb).all(|(a, b)| a.abs_diff(b) <= 12)
}

fn start_xvfb() -> (tokio::process::Child, String) {
    for n in 90..120 {
        if std::path::Path::new(&format!("/tmp/.X11-unix/X{n}")).exists() {
            continue;
        }
        let child = tokio::process::Command::new("Xvfb")
            .args([&format!(":{n}"), "-screen", "0", "800x600x24", "-nolisten", "tcp"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("Xvfb must be installed for this test");
        return (child, format!(":{n}"));
    }
    panic!("no free X display");
}

#[tokio::test]
async fn view_and_control_a_real_x11_desktop() {
    let (_xvfb, display) = start_xvfb();
    // SAFETY: this test binary has a single test, set before any thread reads DISPLAY.
    unsafe { std::env::set_var("DISPLAY", &display) };
    let (conn, screen) = eventually("X server up", || async { x11rb::connect(Some(&display)).ok() }).await;
    let root = conn.setup().roots[screen].root;

    // A full-screen window in a known color that records input.
    let win = conn.generate_id().unwrap();
    conn.create_window(
        0,
        win,
        root,
        0,
        0,
        800,
        600,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new()
            .background_pixel(0x00_e0_a0_30)
            .event_mask(EventMask::KEY_PRESS | EventMask::BUTTON_PRESS),
    )
    .unwrap();
    conn.map_window(win).unwrap();
    conn.flush().unwrap();
    conn.set_input_focus(InputFocus::POINTER_ROOT, win, x11rb::CURRENT_TIME).unwrap();
    conn.flush().unwrap();

    let env = TestEnv::start().await;
    let admin = env.signup("MSP", "o@m.test").await;
    let desktop = irmm_desktop::platform::native().expect("native desktop");
    let (_agent, dev, _dir) = enrolled_agent_with(&env, &admin, desktop).await;
    let d = admin.get(&format!("/api/devices/{dev}")).await.expect_ok();
    assert_eq!(d["inventory"]["monitors"][0]["width"], 800, "{d}");

    let s = admin.post(&format!("/api/devices/{dev}/sessions"), json!({"kind": "rdp", "monitor": 0})).await.expect_ok();
    let ticket = s["ticket"].as_str().unwrap();
    let stream = ws_stream(&gateway_url(&env, dev, ticket)).await.unwrap();
    let mut rdp = RdpClient::connect(stream, &dev.to_string(), ticket).await.expect("rdp connect");
    assert_eq!(rdp.size(), (800, 600));
    rdp.pump_until("screen content", |c| close(c.pixel(400, 300), [0xe0, 0xa0, 0x30])).await.unwrap();

    rdp.send([
        Operation::MouseMove(MousePosition { x: 123, y: 456 }),
        Operation::MouseButtonPressed(MouseButton::Left),
        Operation::MouseButtonReleased(MouseButton::Left),
        Operation::KeyPressed(Scancode::from_u8(false, 0x1E)), // A
        Operation::KeyReleased(Scancode::from_u8(false, 0x1E)),
    ])
    .await
    .unwrap();

    let pointer = eventually("pointer moved", || async {
        let p = conn.query_pointer(root).unwrap().reply().unwrap();
        (p.root_x == 123 && p.root_y == 456).then_some(p)
    })
    .await;
    assert_eq!((pointer.root_x, pointer.root_y), (123, 456));

    let (mut clicked, mut key) = (false, None);
    eventually("input events", || {
        while let Some(ev) = conn.poll_for_event().unwrap() {
            match ev {
                Event::ButtonPress(b) if b.detail == 1 => clicked = true,
                Event::KeyPress(k) => key = Some(k.detail),
                _ => {}
            }
        }
        let done = clicked && key.is_some();
        async move { done.then_some(()) }
    })
    .await;
    assert_eq!(key, Some(0x1E + 8), "set-1 A → evdev 30 → X keycode 38");

    // The screen follows changes.
    conn.change_window_attributes(win, &x11rb::protocol::xproto::ChangeWindowAttributesAux::new().background_pixel(0x00_10_e0_10))
        .unwrap();
    conn.clear_area(false, win, 0, 0, 0, 0).unwrap();
    conn.flush().unwrap();
    rdp.pump_until("updated screen", |c| close(c.pixel(700, 500), [0x10, 0xe0, 0x10])).await.unwrap();
}

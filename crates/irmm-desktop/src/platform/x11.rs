//! Linux (X11): RandR monitors, GetImage capture, XTest input, arboard clipboard.
//! Pure Rust (x11rb), so the agent binary needs no X libraries on headless servers.
//! Wayland sessions are not supported.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use async_trait::async_trait;
use bytes::Bytes;
use x11rb::connection::Connection as _;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{
    BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt as _, ImageFormat, KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
    MOTION_NOTIFY_EVENT, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::{Clipboard, Desktop, Frame, InputEvent, InputSink, MonitorInfo, MouseButton, ScreenSource, Session};

pub struct X11Desktop;

#[derive(Clone, Debug)]
struct Mon {
    name: String,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    primary: bool,
}

fn connect() -> anyhow::Result<(Arc<RustConnection>, Window)> {
    let (conn, screen) = x11rb::connect(None).context("connect to X display (is DISPLAY set?)")?;
    let root = conn.setup().roots[screen].root;
    Ok((Arc::new(conn), root))
}

fn monitors(conn: &RustConnection, root: Window) -> anyhow::Result<Vec<Mon>> {
    let mut out = Vec::new();
    if let Ok(reply) = conn.randr_get_monitors(root, true).map_err(anyhow::Error::from).and_then(|c| Ok(c.reply()?)) {
        for m in reply.monitors {
            let name = conn.get_atom_name(m.name)?.reply().map(|r| String::from_utf8_lossy(&r.name).into_owned()).unwrap_or_default();
            out.push(Mon { name, x: m.x, y: m.y, width: m.width, height: m.height, primary: m.primary });
        }
    }
    if out.is_empty() {
        let geo = conn.get_geometry(root)?.reply()?;
        out.push(Mon { name: "screen".into(), x: 0, y: 0, width: geo.width, height: geo.height, primary: true });
    }
    // Primary first so index 0 is the main screen.
    out.sort_by_key(|m| !m.primary);
    Ok(out)
}

impl Desktop for X11Desktop {
    fn monitors(&self) -> Vec<MonitorInfo> {
        let Ok((conn, root)) = connect() else { return Vec::new() };
        monitors(&conn, root)
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(i, m)| MonitorInfo {
                index: i as u32,
                name: m.name,
                width: m.width as u32,
                height: m.height as u32,
                primary: m.primary,
            })
            .collect()
    }

    fn open(&self, monitor: u32) -> anyhow::Result<Session> {
        let (conn, root) = connect()?;
        let mon = monitors(&conn, root)?.into_iter().nth(monitor as usize).context("no such monitor")?;
        Ok(Session {
            screen: Box::new(X11Screen { conn: conn.clone(), root, mon: mon.clone(), last: None }),
            input: Box::new(XTestInput { conn, root, mon, spare_keycode: None }),
            clipboard: arboard::Clipboard::new().ok().map(|c| Box::new(ArboardClipboard(c)) as Box<dyn Clipboard>),
        })
    }
}

struct X11Screen {
    conn: Arc<RustConnection>,
    root: Window,
    mon: Mon,
    last: Option<Instant>,
}

const FRAME_INTERVAL: Duration = Duration::from_millis(66);

#[async_trait]
impl ScreenSource for X11Screen {
    fn size(&self) -> (u16, u16) {
        (self.mon.width, self.mon.height)
    }

    async fn next_frame(&mut self) -> anyhow::Result<Frame> {
        if let Some(last) = self.last {
            tokio::time::sleep(FRAME_INTERVAL.saturating_sub(last.elapsed())).await;
        }
        self.last = Some(Instant::now());
        let (conn, root, m) = (self.conn.clone(), self.root, self.mon.clone());
        let image = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
            let reply = conn.get_image(ImageFormat::Z_PIXMAP, root, m.x, m.y, m.width, m.height, !0)?.reply()?;
            Ok(reply.data)
        })
        .await??;
        // 24/32-bit TrueColor ZPixmap on little-endian is B, G, R, X: already BGRA.
        let stride = image.len() / self.mon.height.max(1) as usize;
        anyhow::ensure!(stride >= self.mon.width as usize * 4, "unsupported X visual (need 32 bits per pixel)");
        Ok(Frame { width: self.mon.width, height: self.mon.height, stride, data: Bytes::from(image) })
    }
}

struct XTestInput {
    conn: Arc<RustConnection>,
    root: Window,
    mon: Mon,
    /// A keycode with no symbols, borrowed to type characters missing from the keymap.
    spare_keycode: Option<u8>,
}

impl XTestInput {
    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> anyhow::Result<()> {
        self.conn.xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)?;
        Ok(())
    }

    fn button(&self, button: u8, down: bool) -> anyhow::Result<()> {
        self.fake(if down { BUTTON_PRESS_EVENT } else { BUTTON_RELEASE_EVENT }, button, 0, 0)
    }

    fn click(&self, button: u8, times: i32) -> anyhow::Result<()> {
        for _ in 0..times {
            self.button(button, true)?;
            self.button(button, false)?;
        }
        Ok(())
    }

    fn key(&self, keycode: u8, down: bool) -> anyhow::Result<()> {
        self.fake(if down { KEY_PRESS_EVENT } else { KEY_RELEASE_EVENT }, keycode, 0, 0)
    }

    /// Types one character: uses an existing key (with Shift if needed), otherwise
    /// temporarily maps the character onto a spare keycode.
    fn type_char(&mut self, c: char) -> anyhow::Result<()> {
        let keysym = if (c as u32) < 0x100 { c as u32 } else { 0x0100_0000 | c as u32 };
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let map = self.conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
        let per = map.keysyms_per_keycode as usize;
        let find = |sym: u32| -> Option<(u8, usize)> {
            map.keysyms.chunks(per).enumerate().find_map(|(i, syms)| {
                syms.iter().take(2).position(|&s| s == sym).map(|level| (min + i as u8, level))
            })
        };
        if let Some((code, level)) = find(keysym) {
            let shift = if level == 1 { find(0xffe1).map(|(k, _)| k) } else { None };
            if let Some(s) = shift {
                self.key(s, true)?;
            }
            self.key(code, true)?;
            self.key(code, false)?;
            if let Some(s) = shift {
                self.key(s, false)?;
            }
            return Ok(());
        }
        let spare = match self.spare_keycode {
            Some(k) => k,
            None => {
                let k = map
                    .keysyms
                    .chunks(per)
                    .enumerate()
                    .rev()
                    .find(|(_, syms)| syms.iter().all(|&s| s == 0))
                    .map(|(i, _)| min + i as u8)
                    .context("no spare keycode")?;
                self.spare_keycode = Some(k);
                k
            }
        };
        self.conn.change_keyboard_mapping(1, spare, per as u8, &vec![keysym; per])?;
        self.conn.get_input_focus()?.reply()?; // round trip: mapping applied before the key event
        self.key(spare, true)?;
        self.key(spare, false)?;
        self.conn.get_input_focus()?.reply()?;
        self.conn.change_keyboard_mapping(1, spare, per as u8, &vec![0; per])?;
        Ok(())
    }
}

impl InputSink for XTestInput {
    fn input(&mut self, ev: InputEvent) {
        let res = match ev {
            InputEvent::MouseMove { x, y } => {
                self.fake(MOTION_NOTIFY_EVENT, 0, self.mon.x + x.min(self.mon.width) as i16, self.mon.y + y.min(self.mon.height) as i16)
            }
            InputEvent::Button { button, down } => {
                let b = match button {
                    MouseButton::Left => 1,
                    MouseButton::Middle => 2,
                    MouseButton::Right => 3,
                    MouseButton::X1 => 8,
                    MouseButton::X2 => 9,
                };
                self.button(b, down)
            }
            // Positive is away from the user: button 4 (up) / 6 (left) and 5 / 7.
            InputEvent::Wheel { delta } => self.click(if delta > 0 { 4 } else { 5 }, notches(delta)),
            InputEvent::HWheel { delta } => self.click(if delta > 0 { 7 } else { 6 }, notches(delta)),
            InputEvent::Key { code, extended, down } => match keycode(code, extended) {
                Some(k) => self.key(k, down),
                None => Ok(()),
            },
            InputEvent::Unicode { ch, down: true } => match char::from_u32(ch as u32) {
                Some(c) => self.type_char(c),
                None => Ok(()),
            },
            InputEvent::Unicode { down: false, .. } => Ok(()),
            // No secure attention sequence on X11.
            InputEvent::SecureAttention => Ok(()),
        };
        let res = res.and_then(|_| Ok(self.conn.flush()?));
        if let Err(e) = res {
            tracing::debug!(error = %format!("{e:#}"), ?ev, "inject input");
        }
    }
}

fn notches(delta: i16) -> i32 {
    (delta as i32 / 120).abs().max(1)
}

/// Set-1 scan code → X11 keycode (evdev + 8).
fn keycode(code: u8, extended: bool) -> Option<u8> {
    let evdev: u8 = if !extended {
        code
    } else {
        match code {
            0x1C => 96,  // KP Enter
            0x1D => 97,  // Right Ctrl
            0x35 => 98,  // KP /
            0x37 => 99,  // Print Screen
            0x38 => 100, // Right Alt
            0x47 => 102, // Home
            0x48 => 103, // Up
            0x49 => 104, // Page Up
            0x4B => 105, // Left
            0x4D => 106, // Right
            0x4F => 107, // End
            0x50 => 108, // Down
            0x51 => 109, // Page Down
            0x52 => 110, // Insert
            0x53 => 111, // Delete
            0x5B => 125, // Left Meta
            0x5C => 126, // Right Meta
            0x5D => 127, // Menu
            _ => return None,
        }
    };
    evdev.checked_add(8)
}

struct ArboardClipboard(arboard::Clipboard);

impl Clipboard for ArboardClipboard {
    fn get_text(&mut self) -> Option<String> {
        self.0.get_text().ok()
    }

    fn set_text(&mut self, text: &str) {
        let _ = self.0.set_text(text.to_owned());
    }
}

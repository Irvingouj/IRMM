//! macOS: xcap capture, enigo input, arboard clipboard.

use std::time::{Duration, Instant};

use anyhow::Context as _;
use async_trait::async_trait;
use bytes::Bytes;
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Keyboard as _, Mouse as _, Settings};

use crate::{Clipboard, Desktop, Frame, InputEvent, InputSink, MonitorInfo, MouseButton, ScreenSource, Session};

pub struct XcapDesktop;

fn monitors() -> anyhow::Result<Vec<xcap::Monitor>> {
    let mut all = xcap::Monitor::all().context("enumerate monitors")?;
    // Primary first so index 0 is the main screen.
    all.sort_by_key(|m| !m.is_primary().unwrap_or(false));
    Ok(all)
}

impl Desktop for XcapDesktop {
    fn monitors(&self) -> Vec<MonitorInfo> {
        monitors()
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(i, m)| MonitorInfo {
                index: i as u32,
                name: m.friendly_name().or_else(|_| m.name()).unwrap_or_default(),
                width: m.width().unwrap_or(0),
                height: m.height().unwrap_or(0),
                primary: m.is_primary().unwrap_or(false),
            })
            .collect()
    }

    fn open(&self, monitor: u32) -> anyhow::Result<Session> {
        let m = monitors()?.into_iter().nth(monitor as usize).context("no such monitor")?;
        let first = m.capture_image().context("capture screen (screen recording permission?)")?;
        let (pw, ph) = (first.width(), first.height());
        // Input coordinates are logical (points on macOS); captures are physical pixels.
        let origin = (m.x()?, m.y()?);
        let scale = (m.width()? as f64 / pw as f64, m.height()? as f64 / ph as f64);
        let enigo = Enigo::new(&Settings::default()).map_err(|e| anyhow::anyhow!("input init: {e}"))?;
        Ok(Session {
            screen: Box::new(XcapScreen { monitor: Some(m), size: (pw as u16, ph as u16), last: None }),
            input: Box::new(EnigoInput { enigo, origin, scale }),
            clipboard: arboard::Clipboard::new().ok().map(|c| Box::new(ArboardClipboard(c)) as Box<dyn Clipboard>),
        })
    }
}

struct XcapScreen {
    monitor: Option<xcap::Monitor>,
    size: (u16, u16),
    last: Option<Instant>,
}

const FRAME_INTERVAL: Duration = Duration::from_millis(66);

#[async_trait]
impl ScreenSource for XcapScreen {
    fn size(&self) -> (u16, u16) {
        self.size
    }

    async fn next_frame(&mut self) -> anyhow::Result<Frame> {
        if let Some(last) = self.last {
            tokio::time::sleep(FRAME_INTERVAL.saturating_sub(last.elapsed())).await;
        }
        self.last = Some(Instant::now());
        let m = self.monitor.take().context("monitor gone")?;
        let (m, img) = tokio::task::spawn_blocking(move || {
            let img = m.capture_image();
            (m, img)
        })
        .await?;
        self.monitor = Some(m);
        let img = img.context("capture")?;
        let (w, h) = (img.width() as u16, img.height() as u16);
        let mut data = img.into_raw();
        for px in data.chunks_exact_mut(4) {
            px.swap(0, 2); // RGBA → BGRA
        }
        // A resolution change mid-session is clamped to the negotiated size.
        let (w, h) = (w.min(self.size.0), h.min(self.size.1));
        Ok(Frame { width: w, height: h, stride: img_stride(&data, h), data: Bytes::from(data) })
    }
}

fn img_stride(data: &[u8], h: u16) -> usize {
    data.len() / h.max(1) as usize
}

struct EnigoInput {
    enigo: Enigo,
    origin: (i32, i32),
    scale: (f64, f64),
}

impl InputSink for EnigoInput {
    fn input(&mut self, ev: InputEvent) {
        let res = match ev {
            InputEvent::MouseMove { x, y } => self.enigo.move_mouse(
                self.origin.0 + (x as f64 * self.scale.0) as i32,
                self.origin.1 + (y as f64 * self.scale.1) as i32,
                Coordinate::Abs,
            ),
            InputEvent::Button { button, down } => {
                let b = match button {
                    MouseButton::Left => Button::Left,
                    MouseButton::Right => Button::Right,
                    MouseButton::Middle => Button::Middle,
                    MouseButton::X1 => Button::Back,
                    MouseButton::X2 => Button::Forward,
                };
                self.enigo.button(b, if down { Direction::Press } else { Direction::Release })
            }
            InputEvent::Wheel { delta } => self.enigo.scroll(-notches(delta), Axis::Vertical),
            InputEvent::HWheel { delta } => self.enigo.scroll(notches(delta), Axis::Horizontal),
            InputEvent::Key { code, extended, down } => match keycode(code, extended) {
                Some(k) => self.enigo.raw(k, if down { Direction::Press } else { Direction::Release }),
                None => Ok(()),
            },
            InputEvent::Unicode { ch, down: true } => match char::from_u32(ch as u32) {
                Some(c) => self.enigo.text(&c.to_string()),
                None => Ok(()),
            },
            InputEvent::Unicode { down: false, .. } => Ok(()),
            // No secure attention sequence outside Windows; treat as the plain chord.
            InputEvent::SecureAttention => Ok(()),
        };
        if let Err(e) = res {
            tracing::debug!(error = %e, ?ev, "inject input");
        }
    }
}

fn notches(delta: i16) -> i32 {
    let n = delta as i32 / 120;
    if n == 0 { delta.signum() as i32 } else { n }
}

/// Set-1 scan code → macOS virtual key code (US physical layout).
fn keycode(code: u8, extended: bool) -> Option<u16> {
    if extended {
        return Some(match code {
            0x1C => 0x4C,
            0x1D => 0x3E,
            0x35 => 0x4B,
            0x38 => 0x3D,
            0x47 => 0x73,
            0x48 => 0x7E,
            0x49 => 0x74,
            0x4B => 0x7B,
            0x4D => 0x7C,
            0x4F => 0x77,
            0x50 => 0x7D,
            0x51 => 0x79,
            0x52 => 0x72,
            0x53 => 0x75,
            0x5B => 0x37,
            0x5C => 0x36,
            _ => return None,
        });
    }
    const MAP: [u8; 0x59] = [
        0xFF, 0x35, 0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1A, 0x1C, 0x19, 0x1D, 0x1B, 0x18, 0x33, 0x30, // 0x00
        0x0C, 0x0D, 0x0E, 0x0F, 0x11, 0x10, 0x20, 0x22, 0x1F, 0x23, 0x21, 0x1E, 0x24, 0x3B, 0x00, 0x01, // 0x10
        0x02, 0x03, 0x05, 0x04, 0x26, 0x28, 0x25, 0x29, 0x27, 0x32, 0x38, 0x2A, 0x06, 0x07, 0x08, 0x09, // 0x20
        0x0B, 0x2D, 0x2E, 0x2B, 0x2F, 0x2C, 0x3C, 0x43, 0x3A, 0x31, 0x39, 0x7A, 0x78, 0x63, 0x76, 0x60, // 0x30
        0x61, 0x62, 0x64, 0x65, 0x6D, 0x47, 0xFF, 0x59, 0x5B, 0x5C, 0x4E, 0x56, 0x57, 0x58, 0x45, 0x53, // 0x40
        0x54, 0x55, 0x52, 0x41, 0xFF, 0xFF, 0x0A, 0x67, 0x6F, // 0x50
    ];
    MAP.get(code as usize).copied().filter(|&k| k != 0xFF).map(u16::from)
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

//! Windows: GDI capture and SendInput on a dedicated "desktop thread" that follows
//! the input desktop, so the secure desktop (UAC prompts, lock and logon screens)
//! is visible and controllable when this runs as SYSTEM in the user's session.

use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::oneshot;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
    ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, OpenInputDesktop, SetThreadDesktop,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    KEYEVENTF_UNICODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, MONITORINFOF_PRIMARY, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};
use windows::core::BOOL;

use crate::{Clipboard, Desktop, Frame, InputEvent, InputSink, MonitorInfo, MouseButton, ScreenSource, Session};

pub struct GdiDesktop;

#[derive(Clone, Debug)]
struct Mon {
    rect: RECT,
    primary: bool,
    name: String,
}

fn enum_monitors() -> Vec<Mon> {
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let out = unsafe { &mut *(data.0 as *mut Vec<Mon>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(h, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) }.as_bool() {
            let len = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len());
            out.push(Mon {
                rect: info.monitorInfo.rcMonitor,
                primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
                name: String::from_utf16_lossy(&info.szDevice[..len]),
            });
        }
        BOOL(1)
    }
    let mut out: Vec<Mon> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut out as *mut _ as isize));
    }
    out.sort_by_key(|m| !m.primary);
    out
}

impl Desktop for GdiDesktop {
    fn monitors(&self) -> Vec<MonitorInfo> {
        with_input_desktop(enum_monitors)
            .iter()
            .enumerate()
            .map(|(i, m)| MonitorInfo {
                index: i as u32,
                name: m.name.clone(),
                width: (m.rect.right - m.rect.left) as u32,
                height: (m.rect.bottom - m.rect.top) as u32,
                primary: m.primary,
            })
            .collect()
    }

    fn open(&self, monitor: u32) -> anyhow::Result<Session> {
        let (tx, rx) = std_mpsc::channel::<Cmd>();
        let (ready_tx, ready_rx) = std_mpsc::channel();
        std::thread::Builder::new()
            .name("irmm-desktop".into())
            .spawn(move || desktop_thread(monitor, rx, ready_tx))?;
        let size = ready_rx.recv().context("desktop thread died")??;
        Ok(Session {
            screen: Box::new(GdiScreen { tx: tx.clone(), size, last: None }),
            input: Box::new(SendInputSink { tx }),
            clipboard: arboard::Clipboard::new().ok().map(|c| Box::new(ArboardClipboard(c)) as Box<dyn Clipboard>),
        })
    }
}

enum Cmd {
    Capture(oneshot::Sender<anyhow::Result<Frame>>),
    Input(InputEvent),
}

/// Attaches the calling thread to the current input desktop (Default, Winlogon, …).
fn follow_input_desktop() {
    unsafe {
        if let Ok(desk) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(0x10000000)) {
            if SetThreadDesktop(desk).is_err() {
                let _ = CloseDesktop(desk);
            }
        }
    }
}

fn with_input_desktop<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::spawn(move || {
        follow_input_desktop();
        f()
    })
    .join()
    .expect("desktop helper thread")
}

fn desktop_thread(monitor: u32, rx: std_mpsc::Receiver<Cmd>, ready: std_mpsc::Sender<anyhow::Result<(u16, u16)>>) {
    follow_input_desktop();
    let Some(mon) = enum_monitors().into_iter().nth(monitor as usize) else {
        let _ = ready.send(Err(anyhow::anyhow!("no such monitor")));
        return;
    };
    let (w, h) = (mon.rect.right - mon.rect.left, mon.rect.bottom - mon.rect.top);
    let _ = ready.send(Ok((w as u16, h as u16)));
    while let Ok(cmd) = rx.recv() {
        follow_input_desktop();
        match cmd {
            Cmd::Capture(reply) => {
                let _ = reply.send(capture(&mon.rect));
            }
            Cmd::Input(InputEvent::SecureAttention) => send_sas(),
            Cmd::Input(ev) => inject(&mon.rect, ev),
        }
    }
}

fn capture(r: &RECT) -> anyhow::Result<Frame> {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            bail!("GetDC failed");
        }
        let mem = CreateCompatibleDC(Some(screen));
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(Some(mem), &bmi, DIB_RGB_COLORS, &mut bits, None, 0);
        let result = match dib {
            Ok(dib) => {
                let old = SelectObject(mem, dib.into());
                let ok = BitBlt(mem, 0, 0, w, h, Some(screen), r.left, r.top, SRCCOPY | CAPTUREBLT);
                let data = if ok.is_ok() {
                    let len = (w * h * 4) as usize;
                    let mut v = std::slice::from_raw_parts(bits as *const u8, len).to_vec();
                    // GDI leaves alpha at 0; RDP ignores it but keep it opaque.
                    for px in v.chunks_exact_mut(4) {
                        px[3] = 0xff;
                    }
                    Ok(v)
                } else {
                    Err(anyhow::anyhow!("BitBlt failed"))
                };
                SelectObject(mem, old);
                let _ = DeleteObject(dib.into());
                data
            }
            Err(e) => Err(anyhow::anyhow!("CreateDIBSection: {e}")),
        };
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        let data = result?;
        Ok(Frame { width: w as u16, height: h as u16, stride: w as usize * 4, data: Bytes::from(data) })
    }
}

fn send_inputs(inputs: &[INPUT]) {
    unsafe {
        SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

fn mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn key(scan: u16, flags: windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn inject(mon: &RECT, ev: InputEvent) {
    match ev {
        InputEvent::MouseMove { x, y } => unsafe {
            let (vx, vy) = (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN));
            let (vw, vh) = (GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1), GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1));
            let px = mon.left + x as i32 - vx;
            let py = mon.top + y as i32 - vy;
            let nx = ((px as i64 * 65535) / (vw - 1).max(1) as i64) as i32;
            let ny = ((py as i64 * 65535) / (vh - 1).max(1) as i64) as i32;
            send_inputs(&[mouse(MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK, nx, ny, 0)]);
        },
        InputEvent::Button { button, down } => {
            let (flags, data) = match (button, down) {
                (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                (MouseButton::X1, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, 1),
                (MouseButton::X2, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, 2),
            };
            send_inputs(&[mouse(flags, 0, 0, data)]);
        }
        InputEvent::Wheel { delta } => send_inputs(&[mouse(MOUSEEVENTF_WHEEL, 0, 0, delta as i32)]),
        InputEvent::HWheel { delta } => send_inputs(&[mouse(MOUSEEVENTF_HWHEEL, 0, 0, delta as i32)]),
        InputEvent::Key { code, extended, down } => {
            let mut flags = KEYEVENTF_SCANCODE;
            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if !down {
                flags |= KEYEVENTF_KEYUP;
            }
            send_inputs(&[key(code as u16, flags)]);
        }
        InputEvent::Unicode { ch, down } => {
            let mut flags = KEYEVENTF_UNICODE;
            if !down {
                flags |= KEYEVENTF_KEYUP;
            }
            send_inputs(&[key(ch, flags)]);
        }
        InputEvent::SecureAttention => send_sas(),
    }
}

/// Ctrl+Alt+Del. Requires LocalSystem and the `SoftwareSASGeneration` policy (set by the installer).
fn send_sas() {
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::core::{s, w};
    unsafe {
        let Ok(lib) = LoadLibraryW(w!("sas.dll")) else {
            tracing::warn!("sas.dll not available");
            return;
        };
        if let Some(f) = GetProcAddress(lib, s!("SendSAS")) {
            let send_sas: extern "system" fn(BOOL) = std::mem::transmute(f);
            send_sas(BOOL(0));
        }
    }
}

struct GdiScreen {
    tx: std_mpsc::Sender<Cmd>,
    size: (u16, u16),
    last: Option<Instant>,
}

const FRAME_INTERVAL: Duration = Duration::from_millis(66);

#[async_trait]
impl ScreenSource for GdiScreen {
    fn size(&self) -> (u16, u16) {
        self.size
    }

    async fn next_frame(&mut self) -> anyhow::Result<Frame> {
        if let Some(last) = self.last {
            tokio::time::sleep(FRAME_INTERVAL.saturating_sub(last.elapsed())).await;
        }
        self.last = Some(Instant::now());
        let (tx, rx) = oneshot::channel();
        self.tx.send(Cmd::Capture(tx)).context("desktop thread gone")?;
        let mut frame = rx.await.context("desktop thread gone")??;
        frame.width = frame.width.min(self.size.0);
        frame.height = frame.height.min(self.size.1);
        Ok(frame)
    }
}

struct SendInputSink {
    tx: std_mpsc::Sender<Cmd>,
}

impl InputSink for SendInputSink {
    fn input(&mut self, ev: InputEvent) {
        let _ = self.tx.send(Cmd::Input(ev));
    }
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

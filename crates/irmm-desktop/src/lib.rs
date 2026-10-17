//! RDP host: turns a [`ScreenSource`] + [`InputSink`] into an `ironrdp-server`
//! session over any byte stream (TLS security, no CredSSP — authorization happens
//! before the stream reaches us).

pub mod clipboard;
pub mod platform;

pub use clipboard::Clipboard;

use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use async_trait::async_trait;
use bytes::Bytes;
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, KeyboardEvent, MouseEvent, PixelFormat, RdpServer, RdpServerDisplay,
    RdpServerDisplayUpdates, RdpServerInputHandler,
};
use tokio::io::{AsyncRead, AsyncWrite};

/// One captured frame, 32-bit BGRA (byte order B, G, R, A), top-down.
#[derive(Clone)]
pub struct Frame {
    pub width: u16,
    pub height: u16,
    pub stride: usize,
    pub data: Bytes,
}

#[async_trait]
pub trait ScreenSource: Send {
    fn size(&self) -> (u16, u16);
    /// Next frame; may block until the screen changes or a capture interval elapses.
    async fn next_frame(&mut self) -> anyhow::Result<Frame>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    /// PC/AT set-1 scan code.
    Key { code: u8, extended: bool, down: bool },
    Unicode { ch: u16, down: bool },
    MouseMove { x: u16, y: u16 },
    Button { button: MouseButton, down: bool },
    /// Positive is away from the user.
    Wheel { delta: i16 },
    HWheel { delta: i16 },
    /// Ctrl+Alt+Del: on Windows this must become a Secure Attention Sequence.
    SecureAttention,
}

pub trait InputSink: Send {
    fn input(&mut self, ev: InputEvent);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInfo {
    pub index: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// A machine's desktop: enumerates monitors and opens a capture + input pair.
pub trait Desktop: Send + Sync {
    fn monitors(&self) -> Vec<MonitorInfo>;
    fn open(&self, monitor: u32) -> anyhow::Result<Session>;
}

/// Everything one RDP session needs from the local machine.
pub struct Session {
    pub screen: Box<dyn ScreenSource>,
    pub input: Box<dyn InputSink>,
    pub clipboard: Option<Box<dyn Clipboard>>,
}

/// Self-signed TLS identity for the RDP server.
pub fn tls_acceptor() -> anyhow::Result<tokio_rustls::TlsAcceptor> {
    let cert = rcgen::generate_simple_self_signed(vec!["irmm-desktop".to_owned()])?;
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into());
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.der().clone()], key)?;
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

/// Runs [`serve`] on its own thread: the ironrdp-server session future is not `Send`.
pub fn spawn<S>(stream: S, session: Session) -> std::thread::JoinHandle<anyhow::Result<()>>
where
    S: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static,
{
    std::thread::Builder::new()
        .name("irmm-rdp".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            let res = rt.block_on(serve(stream, session));
            if let Err(e) = &res {
                tracing::debug!(error = %format!("{e:#}"), "rdp host ended");
            }
            res
        })
        .expect("spawn rdp thread")
}

/// Serves one RDP connection over `stream` until the client disconnects.
pub async fn serve<S>(stream: S, session: Session) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Send + Sync + Unpin,
{
    let Session { screen, input, clipboard } = session;
    let clipboard = clipboard.map(clipboard::Factory::new);
    let watcher = clipboard.as_ref().map(|c| tokio::spawn(c.watch()));
    let mut server = RdpServer::builder()
        .with_addr(([127, 0, 0, 1], 0))
        .with_tls(tls_acceptor()?)
        .with_input_handler(Input::new(input))
        .with_display_handler(Display { source: Some(screen) })
        .with_cliprdr_factory(clipboard.map(|c| Box::new(c) as Box<dyn ironrdp_server::CliprdrServerFactory>))
        .build();
    let res = server.run_connection(stream).await.context("rdp session");
    if let Some(w) = watcher {
        w.abort();
    }
    res
}

struct Display {
    source: Option<Box<dyn ScreenSource>>,
}

#[async_trait]
impl RdpServerDisplay for Display {
    async fn size(&mut self) -> DesktopSize {
        let (width, height) = self.source.as_ref().map_or((1024, 768), |s| s.size());
        DesktopSize { width, height }
    }

    async fn updates(&mut self) -> anyhow::Result<Box<dyn RdpServerDisplayUpdates>> {
        let source = self.source.take().context("display updates requested twice")?;
        Ok(Box::new(Updates { source, last: None }))
    }
}

struct Updates {
    source: Box<dyn ScreenSource>,
    last: Option<Bytes>,
}

#[async_trait]
impl RdpServerDisplayUpdates for Updates {
    async fn next_update(&mut self) -> anyhow::Result<Option<DisplayUpdate>> {
        loop {
            let frame = self.source.next_frame().await?;
            if self.last.as_ref() == Some(&frame.data) {
                tokio::time::sleep(Duration::from_millis(30)).await;
                continue;
            }
            self.last = Some(frame.data.clone());
            // ironrdp-server diffs against its own framebuffer and only encodes changed tiles.
            return Ok(Some(DisplayUpdate::Bitmap(BitmapUpdate {
                x: 0,
                y: 0,
                width: NonZeroU16::new(frame.width).context("zero width")?,
                height: NonZeroU16::new(frame.height).context("zero height")?,
                format: PixelFormat::BgrA32,
                data: frame.data,
                stride: NonZeroUsize::new(frame.stride).context("zero stride")?,
            })));
        }
    }
}

const SC_CTRL: u8 = 0x1D;
const SC_ALT: u8 = 0x38;
const SC_DEL: u8 = 0x53;

struct Input {
    sink: Box<dyn InputSink>,
    ctrl: bool,
    alt: bool,
}

impl Input {
    fn new(sink: Box<dyn InputSink>) -> Self {
        Self { sink, ctrl: false, alt: false }
    }
}

impl RdpServerInputHandler for Input {
    fn keyboard(&mut self, event: KeyboardEvent) {
        let (code, extended, down) = match event {
            KeyboardEvent::Pressed { code, extended } => (code, extended, true),
            KeyboardEvent::Released { code, extended } => (code, extended, false),
            KeyboardEvent::UnicodePressed(ch) => return self.sink.input(InputEvent::Unicode { ch, down: true }),
            KeyboardEvent::UnicodeReleased(ch) => return self.sink.input(InputEvent::Unicode { ch, down: false }),
            KeyboardEvent::Synchronize(_) => return,
        };
        match code {
            SC_CTRL => self.ctrl = down,
            SC_ALT => self.alt = down,
            SC_DEL if down && self.ctrl && self.alt => return self.sink.input(InputEvent::SecureAttention),
            _ => {}
        }
        self.sink.input(InputEvent::Key { code, extended, down });
    }

    fn mouse(&mut self, event: MouseEvent) {
        use MouseButton::*;
        let ev = match event {
            MouseEvent::Move { x, y } => InputEvent::MouseMove { x, y },
            MouseEvent::LeftPressed => InputEvent::Button { button: Left, down: true },
            MouseEvent::LeftReleased => InputEvent::Button { button: Left, down: false },
            MouseEvent::RightPressed => InputEvent::Button { button: Right, down: true },
            MouseEvent::RightReleased => InputEvent::Button { button: Right, down: false },
            MouseEvent::MiddlePressed => InputEvent::Button { button: Middle, down: true },
            MouseEvent::MiddleReleased => InputEvent::Button { button: Middle, down: false },
            MouseEvent::Button4Pressed => InputEvent::Button { button: X1, down: true },
            MouseEvent::Button4Released => InputEvent::Button { button: X1, down: false },
            MouseEvent::Button5Pressed => InputEvent::Button { button: X2, down: true },
            MouseEvent::Button5Released => InputEvent::Button { button: X2, down: false },
            MouseEvent::VerticalScroll { value } => InputEvent::Wheel { delta: value },
            MouseEvent::Scroll { x, y } => {
                if y != 0 {
                    InputEvent::Wheel { delta: y.clamp(i16::MIN.into(), i16::MAX.into()) as i16 }
                } else {
                    InputEvent::HWheel { delta: x.clamp(i16::MIN.into(), i16::MAX.into()) as i16 }
                }
            }
            MouseEvent::RelMove { .. } => return,
        };
        self.sink.input(ev);
    }
}

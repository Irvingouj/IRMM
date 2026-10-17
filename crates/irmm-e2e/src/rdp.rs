//! Test doubles for the desktop (trait fakes) and a native RDCleanPath RDP client
//! that follows the same protocol steps as ironrdp-web in the browser.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, bail};
use async_trait::async_trait;
use bytes::Bytes;
use ironrdp::connector::{self, ClientConnector, ClientConnectorState, Credentials, Sequence as _};
use ironrdp::core::WriteBuf;
use ironrdp::graphics::image_processing::PixelFormat;
use ironrdp::input::{Database, Operation};
use ironrdp::pdu::gcc::KeyboardType;
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp::pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageOutput};
use ironrdp_tokio::{FramedWrite as _, TokioFramed};
use irmm_desktop::{Desktop, Frame, InputEvent, InputSink, MonitorInfo, ScreenSource};
use tokio::io::{AsyncRead, AsyncWrite};

/// Two fake monitors (640x480 primary, 320x240) filled with a settable color.
#[derive(Clone, Default)]
pub struct FakeDesktop {
    color: Arc<Mutex<[u8; 4]>>,
    events: Arc<Mutex<Vec<InputEvent>>>,
}

impl FakeDesktop {
    /// Sets the fill color as RGB.
    pub fn set_rgb(&self, r: u8, g: u8, b: u8) {
        *self.color.lock().unwrap() = [b, g, r, 0xff];
    }

    pub fn events(&self) -> Vec<InputEvent> {
        self.events.lock().unwrap().clone()
    }
}

const MONITORS: [(u32, u32); 2] = [(640, 480), (320, 240)];

impl Desktop for FakeDesktop {
    fn monitors(&self) -> Vec<MonitorInfo> {
        MONITORS
            .iter()
            .enumerate()
            .map(|(i, (w, h))| MonitorInfo { index: i as u32, name: format!("FAKE-{i}"), width: *w, height: *h, primary: i == 0 })
            .collect()
    }

    fn open(&self, monitor: u32) -> anyhow::Result<(Box<dyn ScreenSource>, Box<dyn InputSink>)> {
        let (w, h) = *MONITORS.get(monitor as usize).context("no such monitor")?;
        Ok((
            Box::new(FakeScreen { w: w as u16, h: h as u16, color: self.color.clone() }),
            Box::new(FakeInput(self.events.clone())),
        ))
    }
}

struct FakeScreen {
    w: u16,
    h: u16,
    color: Arc<Mutex<[u8; 4]>>,
}

#[async_trait]
impl ScreenSource for FakeScreen {
    fn size(&self) -> (u16, u16) {
        (self.w, self.h)
    }

    async fn next_frame(&mut self) -> anyhow::Result<Frame> {
        tokio::time::sleep(Duration::from_millis(30)).await;
        let c = *self.color.lock().unwrap();
        let data: Vec<u8> = c.iter().copied().cycle().take(self.w as usize * self.h as usize * 4).collect();
        Ok(Frame { width: self.w, height: self.h, stride: self.w as usize * 4, data: Bytes::from(data) })
    }
}

struct FakeInput(Arc<Mutex<Vec<InputEvent>>>);

impl InputSink for FakeInput {
    fn input(&mut self, ev: InputEvent) {
        self.0.lock().unwrap().push(ev);
    }
}

pub struct RdpClient<S> {
    framed: TokioFramed<S>,
    stage: ActiveStage,
    pub image: DecodedImage,
    input: Database,
}

fn client_config() -> connector::Config {
    connector::Config {
        credentials: Credentials::UsernamePassword { username: "irmm".into(), password: String::new() },
        domain: None,
        enable_tls: true,
        enable_credssp: false,
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: connector::DesktopSize { width: 1024, height: 768 },
        bitmap: None,
        client_build: 0,
        client_name: "irmm-e2e".into(),
        client_dir: String::new(),
        platform: MajorPlatformType::UNIX,
        enable_server_pointer: false,
        request_data: None,
        autologon: false,
        enable_audio_playback: false,
        compression_type: None,
        pointer_software_rendering: true,
        multitransport_flags: None,
        performance_flags: PerformanceFlags::default(),
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
    }
}

#[derive(Clone, Copy, Debug)]
struct CleanPathHint;

impl ironrdp::pdu::PduHint for CleanPathHint {
    fn find_size(&self, bytes: &[u8]) -> ironrdp::core::DecodeResult<Option<(bool, usize)>> {
        match ironrdp_rdcleanpath::RDCleanPathPdu::detect(bytes) {
            ironrdp_rdcleanpath::DetectionResult::Detected { total_length, .. } => Ok(Some((true, total_length))),
            ironrdp_rdcleanpath::DetectionResult::NotEnoughBytes => Ok(None),
            ironrdp_rdcleanpath::DetectionResult::Failed => Err(ironrdp::core::other_err!("CleanPathHint", "invalid PDU")),
        }
    }
}

impl<S> RdpClient<S>
where
    S: AsyncRead + AsyncWrite + Send + Sync + Unpin,
{
    /// RDCleanPath handshake (proxy does TLS), then RDP without CredSSP.
    pub async fn connect(stream: S, destination: &str, proxy_auth: &str) -> anyhow::Result<Self> {
        let mut framed = TokioFramed::new(stream);
        let mut connector = ClientConnector::new(client_config(), "127.0.0.1:33899".parse().unwrap());
        let mut buf = WriteBuf::new();
        let ClientConnectorState::ConnectionInitiationSendRequest = connector.state else { bail!("connector state") };
        connector.step_no_input(&mut buf)?;
        let req = ironrdp_rdcleanpath::RDCleanPathPdu::new_request(buf.filled().to_vec(), destination.into(), proxy_auth.into(), None)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        framed.write_all(&req.to_der().map_err(|e| anyhow::anyhow!("{e}"))?).await?;

        let rsp = framed.read_by_hint(&CleanPathHint).await.context("read RDCleanPath response")?;
        let rsp = ironrdp_rdcleanpath::RDCleanPathPdu::from_der(&rsp).map_err(|e| anyhow::anyhow!("{e}"))?;
        let (x224, chain) = match rsp.into_enum().map_err(|e| anyhow::anyhow!("{e:?}"))? {
            ironrdp_rdcleanpath::RDCleanPath::Response { x224_connection_response, server_cert_chain, .. } => {
                (x224_connection_response, server_cert_chain)
            }
            ironrdp_rdcleanpath::RDCleanPath::GeneralErr(e) => bail!("RDCleanPath error: {e}"),
            other => bail!("unexpected RDCleanPath reply: {other:?}"),
        };
        buf.clear();
        connector.step(x224.as_bytes(), &mut buf)?;
        use x509_cert::der::Decode as _;
        let cert = x509_cert::Certificate::from_der(chain.first().context("no cert")?.as_bytes())?;
        let public_key = cert
            .tbs_certificate
            .subject_public_key_info
            .subject_public_key
            .as_bytes()
            .context("public key")?
            .to_owned();
        let should_upgrade = ironrdp_tokio::skip_connect_begin(&mut connector);
        let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);
        let result = ironrdp_tokio::connect_finalize(
            upgraded,
            connector,
            &mut framed,
            &mut ironrdp_tokio::reqwest::ReqwestNetworkClient::new(),
            "irmm-desktop".into(),
            public_key,
            None,
        )
        .await?;
        let image = DecodedImage::new(PixelFormat::RgbA32, result.desktop_size.width, result.desktop_size.height);
        let stage = ironrdp::session::ActiveStageBuilder {
            static_channels: result.static_channels,
            user_channel_id: result.user_channel_id,
            io_channel_id: result.io_channel_id,
            message_channel_id: result.message_channel_id,
            share_id: result.share_id,
            compression_type: None,
            enable_server_pointer: result.enable_server_pointer,
            pointer_software_rendering: result.pointer_software_rendering,
        }
        .build();
        Ok(Self { framed, stage, image, input: Database::new() })
    }

    pub fn size(&self) -> (u16, u16) {
        (self.image.width(), self.image.height())
    }

    /// RGBA at (x, y).
    pub fn pixel(&self, x: usize, y: usize) -> [u8; 4] {
        let i = (y * self.image.width() as usize + x) * 4;
        self.image.data()[i..i + 4].try_into().unwrap()
    }

    /// Processes server PDUs until `pred` holds.
    pub async fn pump_until(&mut self, what: &str, mut pred: impl FnMut(&Self) -> bool) -> anyhow::Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !pred(self) {
            let (action, payload) = tokio::time::timeout_at(deadline, self.framed.read_pdu())
                .await
                .with_context(|| format!("timed out waiting for {what}"))??;
            for out in self.stage.process(&mut self.image, action, &payload)? {
                match out {
                    ActiveStageOutput::ResponseFrame(f) => self.framed.write_all(&f).await?,
                    ActiveStageOutput::Terminate(r) => bail!("server terminated: {r:?}"),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    pub async fn send(&mut self, ops: impl IntoIterator<Item = Operation>) -> anyhow::Result<()> {
        let events = self.input.apply(ops);
        for out in self.stage.process_fastpath_input(&mut self.image, &events)? {
            if let ActiveStageOutput::ResponseFrame(f) = out {
                self.framed.write_all(&f).await?;
            }
        }
        Ok(())
    }
}

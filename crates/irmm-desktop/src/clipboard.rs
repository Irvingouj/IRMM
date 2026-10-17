//! Text clipboard over the RDP `CLIPRDR` channel.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest, FileContentsResponse,
    FormatDataRequest, FormatDataResponse, LockDataId, OwnedFormatDataResponse,
};
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::mpsc::UnboundedSender;

/// Local text clipboard of the machine being controlled.
pub trait Clipboard: Send {
    fn get_text(&mut self) -> Option<String>;
    fn set_text(&mut self, text: &str);
}

type Sender = Arc<Mutex<Option<UnboundedSender<ServerEvent>>>>;

#[derive(Default)]
struct Shared {
    /// Last text we know both sides agree on; avoids echoing remote pastes back.
    synced: Option<String>,
    /// CLIPRDR initialization finished; sending earlier would fail the session.
    ready: bool,
}

pub struct Factory {
    clipboard: Arc<Mutex<Box<dyn Clipboard>>>,
    sender: Sender,
    shared: Arc<Mutex<Shared>>,
}

impl Factory {
    pub fn new(clipboard: Box<dyn Clipboard>) -> Self {
        Self { clipboard: Arc::new(Mutex::new(clipboard)), sender: Default::default(), shared: Default::default() }
    }

    /// Polls the local clipboard and announces changes to the client.
    pub fn watch(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let (clipboard, sender, shared) = (self.clipboard.clone(), self.sender.clone(), self.shared.clone());
        async move {
            loop {
                tokio::time::sleep(Duration::from_millis(400)).await;
                let text = clipboard.lock().unwrap().get_text();
                let Some(text) = text else { continue };
                let mut sh = shared.lock().unwrap();
                if !sh.ready || sh.synced.as_deref() == Some(text.as_str()) {
                    continue;
                }
                let Some(tx) = sender.lock().unwrap().clone() else { continue };
                sh.synced = Some(text);
                let _ = tx.send(ServerEvent::Clipboard(ClipboardMessage::SendInitiateCopy(vec![ClipboardFormat::new(
                    ClipboardFormatId::CF_UNICODETEXT,
                )])));
            }
        }
    }
}

impl ServerEventSender for Factory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *self.sender.lock().unwrap() = Some(sender);
    }
}

impl CliprdrBackendFactory for Factory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        Box::new(Backend { clipboard: self.clipboard.clone(), sender: self.sender.clone(), shared: self.shared.clone() })
    }
}

impl CliprdrServerFactory for Factory {}

struct Backend {
    clipboard: Arc<Mutex<Box<dyn Clipboard>>>,
    sender: Sender,
    shared: Arc<Mutex<Shared>>,
}

impl core::fmt::Debug for Backend {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("irmm clipboard backend")
    }
}

impl Backend {
    fn send(&self, msg: ClipboardMessage) {
        if let Some(tx) = self.sender.lock().unwrap().as_ref() {
            let _ = tx.send(ServerEvent::Clipboard(msg));
        }
    }
}

ironrdp_core::impl_as_any!(Backend);

impl CliprdrBackend for Backend {
    fn temporary_directory(&self) -> &str {
        ".cliprdr"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::empty()
    }

    fn on_ready(&mut self) {
        self.shared.lock().unwrap().ready = true;
    }

    fn on_request_format_list(&mut self) {
        if self.shared.lock().unwrap().synced.is_some() {
            self.send(ClipboardMessage::SendInitiateCopy(vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]));
        }
    }

    fn on_process_negotiated_capabilities(&mut self, _capabilities: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        if available_formats.iter().any(|f| f.id() == ClipboardFormatId::CF_UNICODETEXT) {
            self.send(ClipboardMessage::SendInitiatePaste(ClipboardFormatId::CF_UNICODETEXT));
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let text = if request.format == ClipboardFormatId::CF_UNICODETEXT {
            self.clipboard.lock().unwrap().get_text()
        } else {
            None
        };
        let rsp = match text {
            Some(t) => OwnedFormatDataResponse::new_unicode_string(&t),
            None => OwnedFormatDataResponse::new_error(),
        };
        self.send(ClipboardMessage::SendFormatData(rsp));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        if response.is_error() {
            return;
        }
        if let Ok(text) = response.to_unicode_string() {
            self.shared.lock().unwrap().synced = Some(text.clone());
            self.clipboard.lock().unwrap().set_text(&text);
        }
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}

    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}

    fn on_lock(&mut self, _data_id: LockDataId) {}

    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

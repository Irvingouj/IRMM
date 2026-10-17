//! RDCleanPath responder. The browser (ironrdp-web) speaks RDCleanPath; this side
//! plays the proxy role: forwards X.224, does TLS with the local RDP server, returns
//! the server certificate chain, then relays plaintext RDP into the TLS stream.

use std::sync::Arc;

use anyhow::{Context as _, bail};
use ironrdp_rdcleanpath::{DetectionResult, RDCleanPath, RDCleanPathPdu};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::{BoxRead, BoxWrite};
use crate::Ctx;

pub async fn serve(ctx: Ctx, monitor: u32, mut r: BoxRead, mut w: BoxWrite) -> anyhow::Result<()> {
    let req = read_cleanpath(&mut r).await?;
    let x224_req = match req.into_enum().map_err(|e| anyhow::anyhow!("{e:?}"))? {
        RDCleanPath::Request { x224_connection_request, .. } => x224_connection_request,
        _ => bail!("expected RDCleanPath request"),
    };

    let mut server = match ctx.desktop.connect(monitor).await {
        Ok(s) => s,
        Err(e) => {
            send_pdu(&mut w, &RDCleanPathPdu::new_http_error(503)).await?;
            return Err(e.context("connect desktop"));
        }
    };
    server.write_all(x224_req.as_bytes()).await?;
    let x224_rsp = read_tpkt(&mut server).await.context("read X.224 confirm")?;

    let tls = tokio_rustls::TlsConnector::from(Arc::new(insecure_client_config()));
    let tls_stream = tls
        .connect(rustls::pki_types::ServerName::try_from("irmm-desktop")?, server)
        .await
        .context("TLS with desktop")?;
    let chain: Vec<Vec<u8>> = tls_stream
        .get_ref()
        .1
        .peer_certificates()
        .context("no desktop certificate")?
        .iter()
        .map(|c| c.to_vec())
        .collect();
    let rsp = RDCleanPathPdu::new_response("127.0.0.1:3389".to_owned(), x224_rsp, chain).map_err(|e| anyhow::anyhow!("{e}"))?;
    send_pdu(&mut w, &rsp).await?;

    let mut client = tokio::io::join(r, w);
    let mut tls_stream = tls_stream;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut tls_stream).await;
    Ok(())
}

async fn read_cleanpath(r: &mut BoxRead) -> anyhow::Result<RDCleanPathPdu> {
    let mut buf = Vec::with_capacity(1024);
    loop {
        match RDCleanPathPdu::detect(&buf) {
            DetectionResult::Detected { total_length, .. } if buf.len() >= total_length => {
                return RDCleanPathPdu::from_der(&buf[..total_length]).map_err(|e| anyhow::anyhow!("decode RDCleanPath: {e}"));
            }
            DetectionResult::Failed => bail!("not an RDCleanPath PDU"),
            _ => {}
        }
        if buf.len() > 64 * 1024 {
            bail!("RDCleanPath PDU too large");
        }
        let mut chunk = [0u8; 4096];
        let n = r.read(&mut chunk).await?;
        if n == 0 {
            bail!("EOF before RDCleanPath PDU");
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Reads one TPKT-framed PDU (X.224 connection confirm).
async fn read_tpkt<S: tokio::io::AsyncRead + Unpin>(s: &mut S) -> anyhow::Result<Vec<u8>> {
    let mut hdr = [0u8; 4];
    s.read_exact(&mut hdr).await?;
    if hdr[0] != 3 {
        bail!("not a TPKT header");
    }
    let len = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
    if len < 4 {
        bail!("bad TPKT length");
    }
    let mut pdu = hdr.to_vec();
    pdu.resize(len, 0);
    s.read_exact(&mut pdu[4..]).await?;
    Ok(pdu)
}

async fn send_pdu(w: &mut BoxWrite, pdu: &RDCleanPathPdu) -> anyhow::Result<()> {
    w.write_all(&pdu.to_der().map_err(|e| anyhow::anyhow!("{e}"))?).await?;
    w.flush().await?;
    Ok(())
}

/// The desktop server is local and authenticated by the channel ticket; its
/// certificate is self-signed and only handed to the browser for CredSSP-free RDP.
fn insecure_client_config() -> rustls::ClientConfig {
    #[derive(Debug)]
    struct AcceptAny(Arc<rustls::crypto::CryptoProvider>);
    impl rustls::client::danger::ServerCertVerifier for AcceptAny {
        fn verify_server_cert(
            &self,
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &[rustls::pki_types::CertificateDer<'_>],
            _: &rustls::pki_types::ServerName<'_>,
            _: &[u8],
            _: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            m: &[u8],
            c: &rustls::pki_types::CertificateDer<'_>,
            d: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
        }
        fn verify_tls13_signature(
            &self,
            m: &[u8],
            c: &rustls::pki_types::CertificateDer<'_>,
            d: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny(provider)))
        .with_no_client_auth()
}

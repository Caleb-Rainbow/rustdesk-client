//! Certificate-verified WSS transport and bounded RFC 6455 data frames.
//!
//! A frame limit bounds WebSocket payloads, not TCP packets or TLS records.
//! TLS implementations remain identifiable; this does not imitate a browser.

use anyhow::{bail, Result};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
#[cfg(not(target_os = "windows"))]
use std::sync::Arc;
use std::{io, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    time::timeout,
};
use tokio_tungstenite::{
    connect_async_tls_with_config,
    tungstenite::{
        client::IntoClientRequest,
        protocol::{
            frame::{
                coding::{Data, OpCode},
                Frame,
            },
            Message, WebSocketConfig,
        },
    },
    Connector, MaybeTlsStream, WebSocketStream,
};

// Smaller than the ID server's 64 KiB frame limit. Fragmentation preserves one
// binary message, so protobuf and encrypted payload boundaries remain intact.
pub(crate) const MAX_FRAME_PAYLOAD: usize = 16 * 1024;

fn connector() -> Result<Connector> {
    #[cfg(target_os = "windows")]
    {
        // native-tls uses Schannel and the Windows certificate store. No unsafe
        // retry, cached verification bypass, or alternate TLS fingerprint.
        let connector = tokio_native_tls::native_tls::TlsConnector::builder()
            .min_protocol_version(Some(tokio_native_tls::native_tls::Protocol::Tlsv12))
            .build()?;
        Ok(Connector::NativeTls(connector))
    }
    #[cfg(not(target_os = "windows"))]
    {
        #[cfg(target_os = "android")]
        if !crate::config::ANDROID_RUSTLS_PLATFORM_VERIFIER_INITIALIZED
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            bail!("Android certificate verifier is not initialized");
        }
        let builder = tokio_rustls::rustls::ClientConfig::builder_with_provider(Arc::new(
            tokio_rustls::rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?;
        let verifier = rustls_platform_verifier::Verifier::new(builder.crypto_provider().clone())?;
        let mut config = builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        // RFC 6455 HTTP Upgrade uses HTTP/1.1. Offering h2 here would require a
        // separate RFC 8441 implementation, which this transport does not use.
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Connector::Rustls(Arc::new(config)))
    }
}

pub(crate) async fn connect(
    url: &str,
    ms_timeout: u64,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
    let request = url.into_client_request()?;
    if request.uri().scheme_str() != Some("wss") {
        bail!("This client requires a certificate-verified wss:// endpoint");
    }
    // One deadline includes TLS configuration, DNS, TCP, TLS, and HTTP Upgrade.
    // Errors return to reconnect policy without accepting invalid certificates.
    timeout(Duration::from_millis(ms_timeout), async {
        let connector = connector()?;
        let (stream, _) = connect_async_tls_with_config(
            request,
            Some(WebSocketConfig::default()),
            false,
            Some(connector),
        )
        .await?;
        Ok(stream)
    })
    .await?
}

#[derive(Default)]
pub(crate) struct SendState {
    incomplete: bool,
}

impl SendState {
    pub(crate) fn check_ready(&self) -> io::Result<()> {
        if self.incomplete {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "WebSocket send was interrupted; reconnect before using this stream",
            ));
        }
        Ok(())
    }

    pub(crate) async fn send_binary<S>(
        &mut self,
        stream: &mut WebSocketStream<S>,
        bytes: Bytes,
        ms_timeout: u64,
    ) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        self.check_ready()?;
        // Set before the first await. Cancellation, timeout, or an I/O error can
        // leave a partial message on the wire or buffered inside tungstenite.
        // Only a fully flushed message makes this connection reusable.
        self.incomplete = true;
        let send = send_binary(stream, bytes);
        if ms_timeout > 0 {
            timeout(Duration::from_millis(ms_timeout), send).await??;
        } else {
            send.await?;
        }
        self.incomplete = false;
        Ok(())
    }
}

async fn send_binary<S>(stream: &mut WebSocketStream<S>, bytes: Bytes) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if bytes.len() <= MAX_FRAME_PAYLOAD {
        stream.send(Message::Binary(bytes)).await?;
        return Ok(());
    }
    let mut offset = 0;
    while offset < bytes.len() {
        let end = (offset + MAX_FRAME_PAYLOAD).min(bytes.len());
        let opcode = if offset == 0 {
            Data::Binary
        } else {
            Data::Continue
        };
        let frame = Frame::message(
            bytes.slice(offset..end),
            OpCode::Data(opcode),
            end == bytes.len(),
        );
        // tungstenite masks each client frame. Do not send separate Binary
        // messages, padding, proactive Ping frames, or artificial data delays.
        stream.send(Message::Frame(frame)).await?;
        offset = end;
    }
    Ok(())
}

pub(crate) async fn next_data<S>(
    stream: &mut WebSocketStream<S>,
    state: &SendState,
) -> Option<io::Result<Message>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if let Err(err) = state.check_ready() {
        return Some(Err(err));
    }
    while let Some(message) = stream.next().await {
        match message {
            Ok(message @ (Message::Binary(_) | Message::Text(_))) => return Some(Ok(message)),
            Ok(Message::Close(_)) => {
                // Reading the Close queues its reply. Flush before returning so
                // the peer can complete the RFC 6455 closing handshake.
                if let Err(err) = stream.flush().await {
                    return Some(Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("WebSocket close handshake error: {}", err),
                    )));
                }
                return None;
            }
            Ok(_) => continue, // Reading again flushes tungstenite's automatic Pong.
            Err(err) => {
                return Some(Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("WebSocket protocol error: {}", err),
                )));
            }
        }
    }
    None
}

//! Socket helpers shared by ingest and egress.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use socket2::{SockRef, TcpKeepalive};
use streamdelay_rtmp::handshake::{ClientHandshake, Progress, ServerHandshake};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

pub trait Stream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Stream for T {}

pub type BoxStream = Pin<Box<dyn Stream>>;

/// Low latency and dead-peer detection for media sockets.
pub fn tune(tcp: &TcpStream) {
    let _ = tcp.set_nodelay(true);
    let ka = TcpKeepalive::new().with_time(Duration::from_secs(15));
    let _ = SockRef::from(tcp).set_tcp_keepalive(&ka);
}

pub fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("ring supports the default protocol versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

fn hs_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// Runs the server side of the handshake. Returns bytes received after it.
pub async fn server_handshake<S: Stream + ?Sized>(s: &mut S) -> io::Result<Bytes> {
    let mut hs = ServerHandshake::new();
    let mut buf = vec![0u8; 4096];
    let mut out = BytesMut::new();
    loop {
        let n = s.read(&mut buf).await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let progress = hs.feed(&buf[..n], &mut out).map_err(hs_err)?;
        if !out.is_empty() {
            s.write_all(&out.split()).await?;
        }
        if let Progress::Done(rest) = progress {
            return Ok(rest);
        }
    }
}

/// Runs the client side of the handshake. Returns bytes received after it.
pub async fn client_handshake<S: Stream + ?Sized>(s: &mut S) -> io::Result<Bytes> {
    let mut hs = ClientHandshake::new();
    let mut out = BytesMut::new();
    hs.start(&mut out);
    s.write_all(&out.split()).await?;
    let mut buf = vec![0u8; 4096];
    loop {
        let n = s.read(&mut buf).await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let progress = hs.feed(&buf[..n], &mut out).map_err(hs_err)?;
        if !out.is_empty() {
            s.write_all(&out.split()).await?;
        }
        if let Progress::Done(rest) = progress {
            return Ok(rest);
        }
    }
}

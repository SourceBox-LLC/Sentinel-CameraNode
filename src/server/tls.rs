// Sentinel CameraNode - Camera streaming node for Sentinel Command Center
// Copyright (C) 2026  SourceBox LLC
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//! HTTPS for the local dashboard and LAN streams.
//!
//! When the local server is reachable from other machines (its bind is
//! not loopback), it serves HTTPS with a certificate the node makes for
//! itself. Before this (2026-10-19 security audit) everything on the LAN
//! was plain HTTP, so anyone on the same network could read the
//! dashboard password, the 30-day session cookie and stream links.
//!
//! - The certificate is self-signed, so a browser warns once before
//!   trusting it. It is made on first start and kept in node.db, the
//!   private key encrypted like the API key, so the browser's "proceed"
//!   keeps working across restarts. Its SHA-256 fingerprint is logged at
//!   start-up, to check against the browser's certificate viewer.
//! - The port is shared. A connection whose first byte starts a TLS
//!   handshake is served over TLS; anything else is plain HTTP, and gets
//!   a 308 to the same address on `https://`, path and stream token
//!   included, so old `http://` bookmarks and links keep working.
//! - `SOURCEBOX_SENTRY_LAN_TLS=0` turns it off, for a node behind a
//!   reverse proxy that does its own TLS.

use std::future::Future;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::TlsAcceptor;

use crate::storage::NodeDatabase;

/// Config keys the certificate lives under in node.db.
const CERT_KEY: &str = "tls_cert_pem";
const PRIVATE_KEY_KEY: &str = "tls_key_pem";

/// How long a connection may take to send its first byte, finish a TLS
/// handshake, or send a plain request's head before it is dropped.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The first byte of every TLS connection: a handshake record.
const TLS_HANDSHAKE_RECORD: u8 = 0x16;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Whether a server bound to `bind` should serve HTTPS: yes whenever
/// other machines can reach it, unless `SOURCEBOX_SENTRY_LAN_TLS=0`.
pub fn wanted_for(bind: &str) -> bool {
    let loopback = bind
        .parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(bind == "localhost");
    if loopback {
        return false;
    }
    !matches!(
        std::env::var("SOURCEBOX_SENTRY_LAN_TLS").as_deref(),
        Ok("0") | Ok("false") | Ok("off") | Ok("no")
    )
}

/// Record, once at start-up, whether this node serves HTTPS. Read by the
/// session cookie (`Secure`), the URLs the node shows, and registration.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// `https` or `http`, for the URLs the node shows and reports.
pub fn scheme() -> &'static str {
    if enabled() {
        "https"
    } else {
        "http"
    }
}

/// The scheme a LAN-reachable dashboard will use, for setup's hints
/// before the server exists.
pub fn lan_scheme() -> &'static str {
    if wanted_for("0.0.0.0") {
        "https"
    } else {
        "http"
    }
}

/// A TLS acceptor for this node's certificate, made and stored on first
/// use. Returns the acceptor and the certificate's SHA-256 fingerprint.
pub fn acceptor(db: &NodeDatabase) -> Result<(TlsAcceptor, String), String> {
    let (certs, key) = certificate(db)?;
    let fingerprint = fingerprint(&certs[0]);
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS versions: {e}"))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("TLS certificate: {e}"))?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok((TlsAcceptor::from(Arc::new(config)), fingerprint))
}

/// This node's certificate and key: the stored pair, or a new one made
/// and stored now.
fn certificate(
    db: &NodeDatabase,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), String> {
    let stored = (
        db.get_config(CERT_KEY).ok().flatten(),
        db.get_config_encrypted(PRIVATE_KEY_KEY).ok().flatten(),
    );
    if let (Some(cert_pem), Some(key_pem)) = stored {
        match parse(&cert_pem, &key_pem) {
            Some(pair) => return Ok(pair),
            None => tracing::warn!("Stored TLS certificate is unreadable; making a new one"),
        }
    }
    let (cert_pem, key_pem) = generate()?;
    db.set_config(CERT_KEY, &cert_pem)
        .map_err(|e| format!("store certificate: {e}"))?;
    db.set_config_encrypted(PRIVATE_KEY_KEY, &key_pem)
        .map_err(|e| format!("store certificate key: {e}"))?;
    parse(&cert_pem, &key_pem).ok_or_else(|| "a new certificate did not parse".to_string())
}

fn parse(
    cert_pem: &str,
    key_pem: &str,
) -> Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let cert = CertificateDer::from_pem_slice(cert_pem.as_bytes()).ok()?;
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).ok()?;
    Some((vec![cert], key))
}

/// A self-signed ECDSA P-256 certificate, valid for ten years, naming
/// this machine's host name, `localhost`, loopback and its LAN address.
/// A browser checks none of that for a certificate it was told to trust,
/// but a matching name keeps the warning to the one about who signed it.
fn generate() -> Result<(String, String), String> {
    use chrono::Datelike;

    let mut names = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    if let Some(host) = sysinfo::System::host_name().filter(|h| is_dns_name(h)) {
        names.push(format!("{host}.local"));
        names.push(host);
    }
    if let Some(ip) = crate::net::lan_ip() {
        names.push(ip);
    }
    names.dedup();

    let mut params =
        rcgen::CertificateParams::new(names).map_err(|e| format!("certificate names: {e}"))?;
    let mut subject = rcgen::DistinguishedName::new();
    subject.push(rcgen::DnType::CommonName, "Sentinel CameraNode");
    subject.push(rcgen::DnType::OrganizationName, "SourceBox LLC");
    params.distinguished_name = subject;
    let today = chrono::Utc::now().date_naive() - chrono::Duration::days(1);
    params.not_before = rcgen::date_time_ymd(today.year(), today.month() as u8, today.day() as u8);
    params.not_after = rcgen::date_time_ymd(
        today.year() + 10,
        today.month() as u8,
        today.day().min(28) as u8,
    );

    let key = rcgen::KeyPair::generate().map_err(|e| format!("certificate key: {e}"))?;
    let cert = params
        .self_signed(&key)
        .map_err(|e| format!("self-sign: {e}"))?;
    Ok((cert.pem(), key.serialize_pem()))
}

fn is_dns_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !name.starts_with('-')
}

/// `AB:CD:…`, the form browsers show in their certificate viewer.
pub fn fingerprint(cert: &CertificateDer<'_>) -> String {
    Sha256::digest(cert.as_ref())
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Serve `listener`: TLS connections go to `handle`, plain HTTP gets a
/// redirect to `https://`. Runs until the process ends.
pub async fn serve<H, Fut>(listener: TcpListener, acceptor: TlsAcceptor, handle: H)
where
    H: Fn(hyper_util::rt::TokioIo<tokio_rustls::server::TlsStream<TcpStream>>) -> Fut
        + Clone
        + Send
        + Sync
        + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let fallback_host = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "localhost".to_string());
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                // Out of file descriptors and the like: back off rather
                // than spin.
                tracing::warn!("Local server accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let handle = handle.clone();
        let fallback_host = fallback_host.clone();
        tokio::spawn(async move {
            let mut first = [0u8; 1];
            let peeked = tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.peek(&mut first)).await;
            if !matches!(peeked, Ok(Ok(1))) {
                return;
            }
            if first[0] == TLS_HANDSHAKE_RECORD {
                match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                    Ok(Ok(tls)) => handle(hyper_util::rt::TokioIo::new(tls)).await,
                    // A browser that has not yet accepted the
                    // certificate aborts the handshake; that is routine.
                    Ok(Err(e)) => tracing::debug!("TLS handshake failed: {e}"),
                    Err(_) => tracing::debug!("TLS handshake timed out"),
                }
            } else {
                redirect_to_https(stream, &fallback_host).await;
            }
        });
    }
}

/// Answer a plain-HTTP request with a 308 to the same place on HTTPS.
async fn redirect_to_https(mut stream: TcpStream, fallback_host: &str) {
    let mut head = Vec::with_capacity(1024);
    let mut buf = [0u8; 1024];
    while head.len() < 8192 && !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => head.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }
    let location = https_location(&String::from_utf8_lossy(&head), fallback_host);
    let response = format!(
        "HTTP/1.1 308 Permanent Redirect\r\nLocation: {location}\r\n\
         Content-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// `https://<Host><path>` for a plain request's head: the request
/// target keeps its query (a stream link's `?st=` token), and the Host
/// header already carries the port. A missing or odd Host falls back to
/// the listener's own address.
pub(crate) fn https_location(head: &str, fallback_host: &str) -> String {
    let mut lines = head.split("\r\n");
    let target = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .filter(|t| t.starts_with('/') && !t.contains(char::is_control))
        .unwrap_or("/");
    let host = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("host"))
        .map(|(_, value)| value.trim())
        .filter(|h| {
            !h.is_empty()
                && h.chars()
                    .all(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(c))
        })
        .unwrap_or(fallback_host);
    format!("https://{host}{target}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_requests_are_sent_to_the_same_place_on_https() {
        let head =
            "GET /hls/cam1/stream.m3u8?st=123.abc HTTP/1.1\r\nHost: 192.168.1.20:8080\r\n\r\n";
        assert_eq!(
            https_location(head, "0.0.0.0:8080"),
            "https://192.168.1.20:8080/hls/cam1/stream.m3u8?st=123.abc"
        );
        // Header names are case-insensitive; IPv6 hosts keep brackets.
        assert_eq!(
            https_location("GET / HTTP/1.1\r\nhOsT: [fe80::1]:8080\r\n\r\n", "x"),
            "https://[fe80::1]:8080/"
        );
        // No Host, or one that could smuggle anything: the listener's own.
        assert_eq!(
            https_location("GET /a HTTP/1.0\r\n\r\n", "10.0.0.5:8080"),
            "https://10.0.0.5:8080/a"
        );
        assert_eq!(
            https_location(
                "GET /a HTTP/1.1\r\nHost: evil.example/@x\r\n\r\n",
                "10.0.0.5:8080"
            ),
            "https://10.0.0.5:8080/a"
        );
        // A target that is not a path is not echoed.
        assert_eq!(
            https_location("GET http://elsewhere/ HTTP/1.1\r\nHost: h:1\r\n\r\n", "x"),
            "https://h:1/"
        );
    }

    #[test]
    fn https_is_for_servers_other_machines_can_reach() {
        assert!(!wanted_for("127.0.0.1"));
        assert!(!wanted_for("::1"));
        assert!(!wanted_for("localhost"));
        if std::env::var("SOURCEBOX_SENTRY_LAN_TLS").is_err() {
            assert!(wanted_for("0.0.0.0"));
            assert!(wanted_for("192.168.1.20"));
        }
    }

    #[test]
    fn the_certificate_is_made_once_and_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let db = NodeDatabase::new(&tmp.path().join("node.db")).unwrap();
        let (_, first) = acceptor(&db).unwrap();
        let (_, second) = acceptor(&db).unwrap();
        assert_eq!(first, second, "a restart must not change the certificate");
        assert_eq!(first.len(), 32 * 3 - 1);
        // The private key is stored encrypted, never as PEM.
        let raw = db.get_config(PRIVATE_KEY_KEY).unwrap().unwrap();
        assert!(!raw.contains("PRIVATE KEY"));
    }

    /// The whole path: one port, TLS served, plain HTTP redirected.
    #[tokio::test]
    async fn one_port_serves_https_and_redirects_http() {
        use warp::Filter;

        let tmp = tempfile::tempdir().unwrap();
        let db = NodeDatabase::new(&tmp.path().join("node.db")).unwrap();
        let (acceptor, _) = acceptor(&db).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let routes = warp::path("ping").map(|| String::from("pong")).boxed();
        let service = warp::service(routes);
        tokio::spawn(serve(listener, acceptor, move |io| {
            let service = service.clone();
            async move {
                let _ = hyper_util::server::conn::auto::Builder::new(
                    hyper_util::rt::TokioExecutor::new(),
                )
                .serve_connection(io, hyper_util::service::TowerToHyperService::new(service))
                .await;
            }
        }));

        // HTTPS answers, with the node's own certificate checked: a
        // client that trusts it, asking for `localhost` (one of the
        // names it carries), completes a verified handshake.
        let cert_pem = db.get_config(CERT_KEY).unwrap().unwrap();
        let client = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(cert_pem.as_bytes()).unwrap())
            .resolve("localhost", addr)
            .build()
            .unwrap();
        let body = client
            .get(format!("https://localhost:{}/ping", addr.port()))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "pong");

        // Plain HTTP on the same port is sent to HTTPS.
        let mut plain = TcpStream::connect(addr).await.unwrap();
        plain
            .write_all(format!("GET /ping?x=1 HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut reply = String::new();
        plain.read_to_string(&mut reply).await.unwrap();
        assert!(reply.starts_with("HTTP/1.1 308"), "{reply}");
        assert!(
            reply.contains(&format!("Location: https://{addr}/ping?x=1\r\n")),
            "{reply}"
        );
    }

    /// Home Assistant plays a LAN stream with FFmpeg, which accepts a
    /// self-signed certificate by default and follows the redirect an
    /// old `http://` link gets. Needs ffmpeg and ffprobe; skipped
    /// without them.
    #[tokio::test]
    async fn ffmpeg_plays_the_https_stream_and_follows_the_redirect() {
        use warp::Filter;

        let have = |tool: &str| {
            std::process::Command::new(tool)
                .arg("-version")
                .output()
                .is_ok()
        };
        if !have("ffmpeg") || !have("ffprobe") {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let hls = tmp.path().join("hls");
        std::fs::create_dir_all(&hls).unwrap();
        let made = std::process::Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=160x120:rate=10",
                "-t",
                "3",
                "-c:v",
                "mpeg2video",
                "-f",
                "hls",
                "-hls_time",
                "1",
                "-hls_list_size",
                "0",
            ])
            .arg(hls.join("stream.m3u8"))
            .status()
            .unwrap();
        assert!(made.success());

        let db = NodeDatabase::new(&tmp.path().join("node.db")).unwrap();
        let (acceptor, _) = acceptor(&db).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let service = warp::service(warp::fs::dir(hls).boxed());
        tokio::spawn(serve(listener, acceptor, move |io| {
            let service = service.clone();
            async move {
                let _ = hyper_util::server::conn::auto::Builder::new(
                    hyper_util::rt::TokioExecutor::new(),
                )
                .serve_connection(io, hyper_util::service::TowerToHyperService::new(service))
                .await;
            }
        }));

        for url in [
            format!("https://{addr}/stream.m3u8"),
            format!("http://{addr}/stream.m3u8"),
        ] {
            let probe = tokio::process::Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-show_entries",
                    "stream=codec_name",
                    "-of",
                    "csv=p=0",
                ])
                .arg(&url)
                .output()
                .await
                .unwrap();
            assert!(
                probe.status.success()
                    && String::from_utf8_lossy(&probe.stdout).contains("mpeg2video"),
                "{url}: {}",
                String::from_utf8_lossy(&probe.stderr)
            );
        }
    }
}

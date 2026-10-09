//! What laugh spends its time on, as `tracing` spans: every call to GitHub,
//! prognost, and opening the PRs.
//!
//! They always go to a log file (each span when it closes, with how long it
//! took), so a slow start can be looked at afterwards. With the standard
//! `OTEL_EXPORTER_OTLP_ENDPOINT` (or `…_TRACES_ENDPOINT`) set they also go
//! to that OpenTelemetry collector over OTLP/HTTP — Jaeger, Grafana, otel-tui
//! or anything else that takes OTLP.
//!
//! The terminal belongs to the UI, so nothing is ever written to it, and a
//! log file or collector that can't be reached never stops laugh.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http::{Request, Response};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_http::{HttpClient, HttpError};
use opentelemetry_otlp::{SpanExporter, WithExportConfig as _, WithHttpConfig as _};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::prelude::*;

/// Past this, the log is moved to `laugh.log.1` when laugh starts.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const DEFAULT_FILTER: &str = "laugh=info";

/// Keeps the collector exporter alive; dropping it sends what's left.
pub struct Guard {
    provider: Option<SdkTracerProvider>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take() {
            let _ = provider.shutdown();
        }
    }
}

/// Where the log goes: `LAUGH_LOG_FILE`, else the platform's place for an
/// app's logs — `~/Library/Logs/laugh/` on macOS, `$XDG_STATE_HOME/laugh/`
/// (`~/.local/state/laugh/`) elsewhere, `%LOCALAPPDATA%\laugh\` on Windows.
pub fn log_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("LAUGH_LOG_FILE").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let dir = if cfg!(target_os = "macos") {
        home()?.join("Library/Logs/laugh")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("laugh")
    } else if let Some(state) = std::env::var_os("XDG_STATE_HOME").filter(|p| !p.is_empty()) {
        PathBuf::from(state).join("laugh")
    } else {
        home()?.join(".local/state/laugh")
    };
    Some(dir.join("laugh.log"))
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

fn open_log() -> Option<File> {
    let path = log_path()?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).ok()?;
    }
    if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let _ = fs::rename(&path, path.with_extension("log.1"));
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}

/// `LAUGH_LOG` picks what's recorded, in `RUST_LOG` syntax (`laugh=debug`
/// adds each `gh` process); `off` turns the log file off.
fn filter() -> EnvFilter {
    EnvFilter::try_from_env("LAUGH_LOG").unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
}

fn otlp_endpoint() -> Option<String> {
    [
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        "OTEL_EXPORTER_OTLP_ENDPOINT",
    ]
    .iter()
    .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
}

pub fn init() -> Guard {
    let file = open_log().map(|f| {
        tracing_subscriber::fmt::layer()
            .with_writer(Mutex::new(f))
            .with_ansi(false)
            .with_thread_names(true)
            .with_span_events(FmtSpan::CLOSE)
            .with_filter(filter())
    });

    let mut problem = None;
    let provider = match otlp_endpoint() {
        Some(endpoint) if endpoint.starts_with("http://") => {
            match SpanExporter::builder()
                .with_http()
                .with_http_client(PlainHttp)
                .with_timeout(Duration::from_secs(5))
                .build()
            {
                Ok(exporter) => {
                    let mut resource = Resource::builder();
                    if std::env::var_os("OTEL_SERVICE_NAME").is_none() {
                        resource = resource.with_service_name("laugh");
                    }
                    Some(
                        SdkTracerProvider::builder()
                            .with_batch_exporter(exporter)
                            .with_resource(resource.build())
                            .build(),
                    )
                }
                Err(e) => {
                    problem = Some(format!("OTLP exporter: {e}"));
                    None
                }
            }
        }
        Some(endpoint) => {
            problem = Some(format!(
                "OTLP endpoint {endpoint}: only http:// is supported (a local collector)"
            ));
            None
        }
        None => None,
    };
    let otel = provider.as_ref().map(|p| {
        tracing_opentelemetry::layer()
            .with_tracer(p.tracer("laugh"))
            .with_filter(filter())
    });

    let _ = tracing_subscriber::registry()
        .with(file)
        .with(otel)
        .try_init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "laugh started");
    if let Some(problem) = problem {
        tracing::warn!("{problem}");
    }
    Guard { provider }
}

/// Just enough HTTP/1.1 to POST spans to a collector on `http://`, so
/// tracing doesn't bring in an HTTP stack, an async runtime and TLS.
#[derive(Debug)]
struct PlainHttp;

#[async_trait]
impl HttpClient for PlainHttp {
    async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        post(&request)
    }
}

fn post(request: &Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
    let uri = request.uri();
    if uri.scheme_str() != Some("http") {
        return Err(format!("not an http:// endpoint: {uri}").into());
    }
    let host = uri.host().ok_or("endpoint has no host")?;
    let port = uri.port_u16().unwrap_or(80);
    let path = uri.path_and_query().map_or("/", |p| p.as_str());
    let timeout = Duration::from_secs(5);
    let addr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or("endpoint host doesn't resolve")?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let body = request.body();
    let mut head = format!(
        "{} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Length: {}\r\nConnection: close\r\n",
        request.method(),
        body.len()
    );
    for (name, value) in request.headers() {
        if name != http::header::CONTENT_LENGTH && name != http::header::HOST {
            head.push_str(&format!(
                "{name}: {}\r\n",
                value.to_str().unwrap_or_default()
            ));
        }
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;

    let mut reply = Vec::new();
    stream.read_to_end(&mut reply)?;
    let status_line = reply.split(|&b| b == b'\n').next().unwrap_or_default();
    let status: u16 = String::from_utf8_lossy(status_line)
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or("collector sent no HTTP status")?;
    Ok(Response::builder().status(status).body(Bytes::new())?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn post_sends_the_body_and_reads_the_status() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = conn.read(&mut buf).unwrap();
            conn.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });
        let request = Request::post(format!("http://127.0.0.1:{port}/v1/traces"))
            .header("content-type", "application/x-protobuf")
            .body(Bytes::from_static(b"spans"))
            .unwrap();
        let response = post(&request).unwrap();
        assert_eq!(response.status(), 202);
        let sent = server.join().unwrap();
        assert!(sent.starts_with("POST /v1/traces HTTP/1.1\r\n"));
        assert!(sent.contains("content-type: application/x-protobuf\r\n"));
        assert!(sent.contains("Content-Length: 5\r\n"));
        assert!(sent.ends_with("\r\n\r\nspans"));
    }

    #[test]
    fn https_is_refused_rather_than_sent_in_the_clear() {
        let request = Request::post("https://collector.example/v1/traces")
            .body(Bytes::new())
            .unwrap();
        assert!(post(&request).is_err());
    }
}

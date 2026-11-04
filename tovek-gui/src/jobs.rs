//! Background work: single-file decompiles, folder runs (the CLI-identical
//! batch/validate drivers) and the embedded HTTP server. Everything runs off
//! the UI thread and reports back over channels polled each frame.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use base64::prelude::*;
use luau_lifter::DecompileOptions;

// ---------------------------------------------------------------------------
// Input decoding
// ---------------------------------------------------------------------------

/// How the raw file bytes were interpreted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputKind {
    /// `--` comment header + base64 blob (UniversalSynSaveInstance text dump),
    /// or a plain base64 string.
    Base64Text,
    /// Raw Luau bytecode bytes.
    RawBytecode,
}

impl InputKind {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Base64Text => "base64 text dump",
            Self::RawBytecode => "raw bytecode",
        }
    }
}

/// The exact decode the folder pipeline applies to USSI text dumps:
/// `grep -v '^--' | tr -d ' \t\r\n' | base64 -d`.
pub fn extract_bytecode_from_text(text: &str) -> Result<Vec<u8>, String> {
    let filtered: String = text
        .lines()
        .filter(|line| !line.starts_with("--"))
        .flat_map(|line| line.chars())
        .filter(|c| !matches!(c, ' ' | '\t' | '\r' | '\n'))
        .collect();
    if filtered.is_empty() {
        return Err("no base64 payload found (only comments/whitespace)".to_owned());
    }
    BASE64_STANDARD
        .decode(filtered.as_bytes())
        .map_err(|error| format!("base64 decode failed: {error}"))
}

/// Read a bytecode input file. Text files holding a base64 blob (optionally
/// behind a `--` comment header) are decoded; anything else is raw bytecode.
/// `force_raw` skips the text path for the rare file that is both.
pub fn load_bytecode_file(path: &Path, force_raw: bool) -> Result<(Vec<u8>, InputKind), String> {
    let bytes =
        std::fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if bytes.is_empty() {
        return Err(format!("{} is empty", path.display()));
    }
    if !force_raw
        && let Ok(text) = std::str::from_utf8(&bytes)
        && let Ok(decoded) = extract_bytecode_from_text(text)
        && !decoded.is_empty()
    {
        return Ok((decoded, InputKind::Base64Text));
    }
    Ok((bytes, InputKind::RawBytecode))
}

// ---------------------------------------------------------------------------
// Single-file decompile
// ---------------------------------------------------------------------------

/// Decode-key plan for one decompile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyPlan {
    /// Try the plain key (1) first, then retry with the Roblox client key
    /// (203) on a deserialize failure — the single-file CLI's behaviour.
    Auto,
    /// One fixed key (203 = Roblox client, 1 = plain Luau, or custom).
    Fixed(u8),
}

pub struct SingleOutcome {
    pub result: Result<String, String>,
    pub used_key: u8,
    pub elapsed: Duration,
}

/// Decompile one script on a dedicated thread with the decompile-sized stack.
pub fn spawn_single(
    bytecode: Vec<u8>,
    key_plan: KeyPlan,
    script_name: Option<String>,
    options: DecompileOptions,
) -> Receiver<SingleOutcome> {
    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("tovek-decompile".into())
        .stack_size(luau_lifter::DECOMPILE_STACK_BYTES)
        .spawn(move || {
            let start = Instant::now();
            let (first, retry) = match key_plan {
                KeyPlan::Auto => (1, Some(203)),
                KeyPlan::Fixed(key) => (key, None),
            };
            let mut used_key = first;
            let mut result = luau_lifter::try_decompile_bytecode_with_options(
                &bytecode,
                first,
                script_name.as_deref(),
                options,
            );
            // Roblox client bytecode (from getscriptbytecode) encodes its
            // opcodes; mirror the CLI's silent retry with the Roblox key.
            if let (Err(error), Some(second)) = (&result, retry)
                && error.starts_with("deserialize:")
                && let Ok(source) = luau_lifter::try_decompile_bytecode_with_options(
                    &bytecode,
                    second,
                    script_name.as_deref(),
                    options,
                )
            {
                used_key = second;
                result = Ok(source);
            }
            let _ = tx.send(SingleOutcome {
                result,
                used_key,
                elapsed: start.elapsed(),
            });
        })
        .expect("spawn the decompile thread");
    rx
}

// ---------------------------------------------------------------------------
// Folder decompile / validate (CLI-identical drivers)
// ---------------------------------------------------------------------------

pub struct FolderParams {
    pub src: PathBuf,
    pub out: PathBuf,
    pub key: u8,
    pub threads: usize,
    pub options: DecompileOptions,
    pub emit_upvalue_analysis: bool,
    pub output_extension: String,
    pub export_manifest: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
    pub cache_max_mib: u64,
}

pub struct ValidateParams {
    pub src: PathBuf,
    pub out: PathBuf,
    pub key: u8,
    pub threads: usize,
    pub options: DecompileOptions,
    pub analyze: Option<PathBuf>,
    pub tool_dir: Option<PathBuf>,
    pub old_solver: bool,
}

pub struct FolderOutcome {
    /// The driver's process exit code: 0 = ok, 1 = failures, 2 = usage/IO.
    pub code: i32,
    pub elapsed: Duration,
    /// `*.{ext}` files found under OUT afterwards (quick success summary).
    pub outputs: usize,
}

/// Run the CLI's `decompile-folder` driver (same code, same output tree).
pub fn spawn_folder(params: FolderParams) -> Receiver<FolderOutcome> {
    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("tovek-folder".into())
        .spawn(move || {
            let start = Instant::now();
            let code = crate::batch::run_with_cache(
                &params.src,
                &params.out,
                params.key,
                params.threads,
                false,
                params.options,
                params.emit_upvalue_analysis,
                &params.output_extension,
                params.export_manifest.as_deref(),
                params.cache_dir.as_deref(),
                params.cache_max_mib,
            );
            let outputs = count_outputs(&params.out, &params.output_extension);
            let _ = tx.send(FolderOutcome {
                code,
                elapsed: start.elapsed(),
                outputs,
            });
        })
        .expect("spawn the folder thread");
    rx
}

/// Run the CLI's `validate-folder` driver (decompile + `luau-analyze` gate).
pub fn spawn_validate(params: ValidateParams) -> Receiver<FolderOutcome> {
    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("tovek-validate".into())
        .spawn(move || {
            let start = Instant::now();
            let code = crate::validate::run(
                &params.src,
                &params.out,
                params.key,
                params.threads,
                false,
                params.options,
                params.analyze.as_deref(),
                params.tool_dir.as_deref(),
                params.old_solver,
            );
            let outputs = count_outputs(&params.out, "luau");
            let _ = tx.send(FolderOutcome {
                code,
                elapsed: start.elapsed(),
                outputs,
            });
        })
        .expect("spawn the validate thread");
    rx
}

fn count_outputs(out: &Path, extension: &str) -> usize {
    walkdir::WalkDir::new(out)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case(extension))
        })
        .count()
}

// ---------------------------------------------------------------------------
// Embedded server
// ---------------------------------------------------------------------------

/// Best-effort LAN address of this device — the address an executor on the
/// same network must POST to when the server binds all interfaces. The UDP
/// "connect" selects the outbound interface without sending a single packet.
pub fn lan_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    socket.connect(("8.8.8.8", 53)).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    if ip.is_loopback() || ip.is_unspecified() {
        None
    } else {
        Some(ip)
    }
}

/// True when something accepts TCP connections on 127.0.0.1:`port`. Used on
/// Android, where the server lives in the `:tovek_server` service process and
/// the UI can only observe it from the outside.
pub fn probe_port(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(250)).is_ok()
}

#[derive(Clone, Debug)]
pub enum ServerEvent {
    Running(SocketAddr),
    Stopped,
    Failed(String),
}

/// A running embedded server: the same router as the standalone `web-server`
/// binary, on its own tokio runtime with decompile-sized thread stacks.
pub struct ServerHandle {
    pub port: u16,
    pub bind_all: bool,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    pub events: Receiver<ServerEvent>,
}

impl ServerHandle {
    pub fn stop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Install rustls' ring crypto provider exactly once per process (required
/// before any `RustlsConfig` is built; the default provider is aws-lc, which
/// this build does not carry).
fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Create (or reuse) a self-signed certificate under `dir`, returning
/// `(cert_path, key_path)`. `sans` are the DNS/IP subject alternative names
/// the certificate should cover. Executors that verify TLS will NOT accept
/// this — it exists for clients that allow self-signed; verified setups use
/// a CA-signed cert via the Custom mode instead.
pub fn ensure_self_signed(dir: &Path, sans: &[String]) -> Result<(PathBuf, PathBuf), String> {
    let cert_path = dir.join("tovek-self-signed.crt");
    let key_path = dir.join("tovek-self-signed.key");
    if cert_path.exists() && key_path.exists() {
        return Ok((cert_path, key_path));
    }
    let certified = rcgen::generate_simple_self_signed(sans.to_vec())
        .map_err(|error| format!("generate certificate: {error}"))?;
    std::fs::create_dir_all(dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    std::fs::write(&cert_path, certified.cert.pem())
        .map_err(|error| format!("write {}: {error}", cert_path.display()))?;
    std::fs::write(&key_path, certified.key_pair.serialize_pem())
        .map_err(|error| format!("write {}: {error}", key_path.display()))?;
    Ok((cert_path, key_path))
}

/// Delete a previously generated self-signed pair so the next start
/// regenerates it (e.g. after the LAN address changed).
pub fn forget_self_signed(dir: &Path) {
    let _ = std::fs::remove_file(dir.join("tovek-self-signed.crt"));
    let _ = std::fs::remove_file(dir.join("tovek-self-signed.key"));
}

/// Minimal plain-HTTP GET (std only — the GUI carries no HTTP client).
/// HTTP/1.0 on purpose: it forbids chunked transfer-encoding, so the body is
/// simply everything after the header block. Raw sockets also bypass
/// Android's cleartext-traffic policy (which only governs the Java stack).
fn http_get(host: &str, path: &str) -> Result<Vec<u8>, String> {
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};

    let addr = (host, 80u16)
        .to_socket_addrs()
        .map_err(|error| format!("resolve {host}: {error}"))?
        .next()
        .ok_or_else(|| format!("resolve {host}: no address"))?;
    let mut stream = TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(10))
        .map_err(|error| format!("connect {host}: {error}"))?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(15)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(15)));
    stream
        .write_all(
            format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nUser-Agent: tovek-gui\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .map_err(|error| format!("send request: {error}"))?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|error| format!("read response: {error}"))?;
    let header_end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("malformed HTTP response")?;
    let status_line = String::from_utf8_lossy(
        &response[..response
            .windows(2)
            .position(|w| w == b"\r\n")
            .unwrap_or(header_end)],
    )
    .into_owned();
    if !status_line.contains(" 200") {
        return Err(format!("GET http://{host}{path}: {status_line}"));
    }
    Ok(response[header_end + 4..].to_vec())
}

/// Download local-ip.co's published wildcard certificate (CA-signed for
/// `*.my.local-ip.co`) and private key into `dir`, writing a fullchain PEM
/// (leaf + intermediate — rustls must serve the chain or clients can't
/// verify). Returns `(cert_path, key_path)`. Let's Encrypt rotates the cert,
/// so re-fetch when it expires (~every 90 days).
pub fn fetch_local_ip_cert(dir: &Path) -> Result<(PathBuf, PathBuf), String> {
    let server = http_get("local-ip.co", "/cert/server.pem")?;
    let chain = http_get("local-ip.co", "/cert/chain.pem")?;
    let key = http_get("local-ip.co", "/cert/server.key")?;
    if !server.starts_with(b"-----BEGIN") || !chain.starts_with(b"-----BEGIN") {
        return Err("local-ip.co returned something that is not a PEM certificate".into());
    }
    if !key.starts_with(b"-----BEGIN") {
        return Err("local-ip.co returned something that is not a PEM key".into());
    }
    std::fs::create_dir_all(dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    let cert_path = dir.join("local-ip-fullchain.pem");
    let key_path = dir.join("local-ip.key");
    let mut fullchain = server;
    if fullchain.last() != Some(&b'\n') {
        fullchain.push(b'\n');
    }
    fullchain.extend_from_slice(&chain);
    std::fs::write(&cert_path, fullchain)
        .map_err(|error| format!("write {}: {error}", cert_path.display()))?;
    std::fs::write(&key_path, key)
        .map_err(|error| format!("write {}: {error}", key_path.display()))?;
    Ok((cert_path, key_path))
}

/// The `*.my.local-ip.co` hostname that resolves to `ip` (dash-encoded).
pub fn local_ip_host(ip: std::net::IpAddr) -> Option<String> {
    match ip {
        std::net::IpAddr::V4(v4) => {
            Some(format!("{}.my.local-ip.co", v4.to_string().replace('.', "-")))
        }
        std::net::IpAddr::V6(_) => None,
    }
}

pub fn start_server(
    port: u16,
    bind_all: bool,
    tls: Option<(PathBuf, PathBuf)>,
) -> ServerHandle {
    let (tx, rx) = channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let result = std::thread::Builder::new()
        .name("tovek-server".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(luau_lifter::DECOMPILE_STACK_BYTES)
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = tx.send(ServerEvent::Failed(format!("tokio runtime: {error}")));
                    return;
                }
            };
            runtime.block_on(async move {
                let router = match web_server::default_router() {
                    Ok(router) => router,
                    Err(error) => {
                        let _ = tx.send(ServerEvent::Failed(format!("server config: {error}")));
                        return;
                    }
                };
                let host = if bind_all { [0, 0, 0, 0] } else { [127, 0, 0, 1] };
                let addr = SocketAddr::from((host, port));

                if let Some((cert, key)) = tls {
                    // HTTPS: axum-server's rustls acceptor over the same router.
                    install_crypto_provider();
                    let config = match axum_server::tls_rustls::RustlsConfig::from_pem_file(
                        &cert, &key,
                    )
                    .await
                    {
                        Ok(config) => config,
                        Err(error) => {
                            let _ = tx.send(ServerEvent::Failed(format!(
                                "load TLS cert/key: {error}"
                            )));
                            return;
                        }
                    };
                    let handle = axum_server::Handle::new();
                    {
                        let handle = handle.clone();
                        tokio::spawn(async move {
                            let _ = shutdown_rx.await;
                            handle.graceful_shutdown(Some(Duration::from_secs(3)));
                        });
                    }
                    {
                        let handle = handle.clone();
                        let tx = tx.clone();
                        tokio::spawn(async move {
                            if let Some(local) = handle.listening().await {
                                let _ = tx.send(ServerEvent::Running(local));
                            }
                        });
                    }
                    let served = axum_server::bind_rustls(addr, config)
                        .handle(handle)
                        .serve(router.into_make_service())
                        .await;
                    let _ = tx.send(match served {
                        Ok(()) => ServerEvent::Stopped,
                        Err(error) => ServerEvent::Failed(error.to_string()),
                    });
                    return;
                }

                let listener = match tokio::net::TcpListener::bind(addr).await {
                    Ok(listener) => listener,
                    Err(error) => {
                        let _ = tx.send(ServerEvent::Failed(format!("bind {addr}: {error}")));
                        return;
                    }
                };
                match listener.local_addr() {
                    Ok(local) => {
                        let _ = tx.send(ServerEvent::Running(local));
                    }
                    Err(error) => {
                        let _ = tx.send(ServerEvent::Failed(error.to_string()));
                        return;
                    }
                }
                let served = axum::serve(listener, router)
                    .with_graceful_shutdown(async move {
                        let _ = shutdown_rx.await;
                    })
                    .await;
                let _ = tx.send(match served {
                    Ok(()) => ServerEvent::Stopped,
                    Err(error) => ServerEvent::Failed(error.to_string()),
                });
            });
        });
    if let Err(error) = result {
        let (tx2, rx2) = channel();
        let _ = tx2.send(ServerEvent::Failed(format!("spawn server thread: {error}")));
        return ServerHandle {
            port,
            bind_all,
            shutdown: None,
            events: rx2,
        };
    }
    ServerHandle {
        port,
        bind_all,
        shutdown: Some(shutdown_tx),
        events: rx,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_extraction_matches_the_bash_pipeline() {
        let text = "-- header\n-- more\naGVs\n bG8 =\t\n";
        assert_eq!(extract_bytecode_from_text(text).unwrap(), b"hello");
        assert!(extract_bytecode_from_text("-- only comments\n").is_err());
    }
}

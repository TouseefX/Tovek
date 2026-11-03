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

pub fn start_server(port: u16, bind_all: bool) -> ServerHandle {
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

//! The Tovek GUI application: tabs for single-file decompiling, the
//! CLI-identical folder drivers, the embedded HTTP server and the executor
//! client scripts. One code path for desktop and Android; the UI is designed
//! to be fully operable without a keyboard (pickers, steppers, presets) since
//! NativeActivity never raises the soft keyboard on its own.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use eframe::egui;
use luau_lifter::{ControlFlowOutputPolicy, DecompileOptions};

use crate::jobs::{self, FolderOutcome, InputKind, KeyPlan, ServerEvent, SingleOutcome};
use crate::picker::{PickKind, Picker};
use crate::scripts;

#[cfg(target_os = "android")]
pub type AndroidHandle = winit::platform::android::activity::AndroidApp;
#[cfg(not(target_os = "android"))]
pub type AndroidHandle = ();

const VERSION: &str = "V2.7";

// ---------------------------------------------------------------------------
// Options state shared by the single and folder tabs
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyChoice {
    /// Try plain (1), fall back to Roblox (203) — single-file CLI behaviour.
    Auto,
    Roblox,
    Plain,
    Custom,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PolicyChoice {
    /// Permit the certified synthetic dispatcher fallback.
    AllowDispatcher,
    /// Fail closed instead of emitting synthetic control flow.
    Strict,
}

#[derive(Clone)]
struct OptionsState {
    key: KeyChoice,
    custom_key: u8,
    dont_reuse_var: bool,
    no_synth_helpers: bool,
    assume_no_nan: bool,
    synthesize_arithmetic_loops: bool,
    compact_style: bool,
    assume_standard_libraries: bool,
    emit_binding_provenance: bool,
    compact_annotations: bool,
    policy: PolicyChoice,
}

impl OptionsState {
    fn for_single() -> Self {
        Self {
            key: KeyChoice::Auto,
            custom_key: 203,
            dont_reuse_var: false,
            no_synth_helpers: false,
            assume_no_nan: false,
            synthesize_arithmetic_loops: false,
            compact_style: false,
            assume_standard_libraries: false,
            emit_binding_provenance: false,
            compact_annotations: false,
            // The library default for single scripts.
            policy: PolicyChoice::AllowDispatcher,
        }
    }

    fn for_folder() -> Self {
        Self {
            key: KeyChoice::Roblox,
            // Ordinary folder runs are fail-closed, like `decompile-folder`.
            policy: PolicyChoice::Strict,
            ..Self::for_single()
        }
    }

    fn key_plan(&self) -> KeyPlan {
        match self.key {
            KeyChoice::Auto => KeyPlan::Auto,
            KeyChoice::Roblox => KeyPlan::Fixed(203),
            KeyChoice::Plain => KeyPlan::Fixed(1),
            KeyChoice::Custom => KeyPlan::Fixed(self.custom_key),
        }
    }

    fn fixed_key(&self) -> u8 {
        match self.key {
            KeyChoice::Auto | KeyChoice::Roblox => 203,
            KeyChoice::Plain => 1,
            KeyChoice::Custom => self.custom_key,
        }
    }

    fn to_options(&self) -> DecompileOptions {
        DecompileOptions {
            dont_reuse_var: self.dont_reuse_var,
            no_synth_helpers: self.no_synth_helpers,
            assume_no_nan: self.assume_no_nan,
            synthesize_arithmetic_loops: self.synthesize_arithmetic_loops,
            compact_style: self.compact_style,
            assume_standard_libraries: self.assume_standard_libraries,
            emit_binding_provenance: self.emit_binding_provenance,
            compact_annotations: self.compact_annotations && self.emit_binding_provenance,
            control_flow_policy: match self.policy {
                PolicyChoice::AllowDispatcher => ControlFlowOutputPolicy::AllowCertifiedDispatcher,
                PolicyChoice::Strict => ControlFlowOutputPolicy::StrictNoSyntheticControl,
            },
        }
    }

    fn key_ui(&mut self, ui: &mut egui::Ui, allow_auto: bool) {
        ui.horizontal_wrapped(|ui| {
            ui.label("Decode key:");
            if allow_auto {
                ui.selectable_value(&mut self.key, KeyChoice::Auto, "Auto")
                    .on_hover_text("Try plain (1) first, retry with the Roblox client key (203) — the CLI default");
            }
            ui.selectable_value(&mut self.key, KeyChoice::Roblox, "Roblox (203)")
                .on_hover_text("Roblox client bytecode from getscriptbytecode — the `-e` flag");
            ui.selectable_value(&mut self.key, KeyChoice::Plain, "Plain (1)")
                .on_hover_text("Unencoded Luau bytecode (`--key 1`)");
            ui.selectable_value(&mut self.key, KeyChoice::Custom, "Custom")
                .on_hover_text("op = op * key % 256 (`--key N`)");
            if self.key == KeyChoice::Custom {
                ui.add(egui::DragValue::new(&mut self.custom_key).speed(1));
            }
        });
    }

    fn flags_ui(&mut self, ui: &mut egui::Ui, folder_mode: bool) {
        ui.checkbox(&mut self.dont_reuse_var, "Don't reuse variable names (--dont-reuse-var)")
            .on_hover_text("Do not reuse generated regular local names across functions in one file; loop header names (i, k, v) remain reusable");
        ui.checkbox(&mut self.no_synth_helpers, "No helper synthesis (--no-synth-helpers)")
            .on_hover_text("Disable P1 terminal-helper synthesis (diagnostic/ablation switch)");
        ui.checkbox(&mut self.assume_no_nan, "Assume no NaN (--assume-no-nan)")
            .on_hover_text("Permit relational negation flips without proving operands non-NaN. Cleaner output, but changes behaviour when either operand is NaN");
        ui.checkbox(
            &mut self.synthesize_arithmetic_loops,
            "Synthesize arithmetic loops (--synthesize-arithmetic-loops)",
        )
        .on_hover_text("Synthesize equivalent fixed-count arithmetic loops (experimental; source loop unknown)");
        ui.checkbox(
            &mut self.assume_standard_libraries,
            "Assume standard libraries (--assume-standard-libraries)",
        )
        .on_hover_text("Write folded constants as math.pi, math.huge and Vector3.new(...) when the script never writes those globals or uses getfenv/setfenv");
        ui.horizontal_wrapped(|ui| {
            ui.label("Style:");
            let mut compact = self.compact_style;
            ui.selectable_value(&mut compact, false, "default")
                .on_hover_text("Branch statements (--style default)");
            ui.selectable_value(&mut compact, true, "compact")
                .on_hover_text("Scalar selects as `if c then a else b` (--style compact)");
            self.compact_style = compact;
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("Control flow:");
            if folder_mode {
                ui.selectable_value(&mut self.policy, PolicyChoice::Strict, "strict (fail closed)")
                    .on_hover_text("The folder driver default: an ok result can never contain the synthetic state-machine dispatcher (--strict-no-synthetic-control)");
                ui.selectable_value(
                    &mut self.policy,
                    PolicyChoice::AllowDispatcher,
                    "allow certified dispatcher",
                )
                .on_hover_text("Diagnostic opt-in: permit the semantics-preserving dispatcher (--allow-certified-dispatcher)");
            } else {
                ui.selectable_value(
                    &mut self.policy,
                    PolicyChoice::AllowDispatcher,
                    "allow certified dispatcher",
                )
                .on_hover_text("The single-file default: permit the semantics-preserving state-machine fallback");
                ui.selectable_value(&mut self.policy, PolicyChoice::Strict, "strict (fail closed)")
                    .on_hover_text("Fail instead of returning synthetic control-flow scaffolding (--strict-no-synthetic-control)");
            }
        });
        if folder_mode {
            ui.checkbox(
                &mut self.emit_binding_provenance,
                "Emit binding provenance (--emit-binding-provenance)",
            )
            .on_hover_text("Detailed SSA/storage lineage under OUT/.tovek-analysis (can produce large files); implies upvalue analysis");
            ui.add_enabled(
                self.emit_binding_provenance,
                egui::Checkbox::new(
                    &mut self.compact_annotations,
                    "Compact annotations (--compact-annotations)",
                ),
            )
            .on_hover_text("Short source labels while retaining full annotation text in metadata (requires provenance)");
        }
    }
}

// ---------------------------------------------------------------------------
// Per-tab state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Single,
    Folder,
    Server,
    Scripts,
    About,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PickTarget {
    SingleInput,
    SingleSaveDir,
    FolderSrc,
    FolderOut,
    FolderManifest,
    FolderCache,
    ValidateAnalyze,
    ValidateToolDir,
    ScriptsDir,
    TlsCert,
    TlsKey,
}

struct SingleTab {
    input_path: String,
    force_raw: bool,
    paste_mode: bool,
    pasted: String,
    script_name: String,
    opts: OptionsState,
    job: Option<Receiver<SingleOutcome>>,
    status: String,
    output: String,
    error: Option<String>,
}

struct FolderTab {
    src: String,
    out: String,
    threads: usize,
    extension_lua: bool,
    emit_upvalue_analysis: bool,
    export_manifest: String,
    cache_dir: String,
    cache_max_mib: u64,
    opts: OptionsState,
    job: Option<Receiver<FolderOutcome>>,
    validating: bool,
    analyze_path: String,
    tool_dir: String,
    old_solver: bool,
    result: Option<(String, bool)>,
}

enum ServerState {
    Stopped,
    Starting,
    Running(std::net::SocketAddr),
    Failed(String),
}

/// How the embedded server terminates TLS.
#[derive(Clone, Copy, Eq, PartialEq)]
enum TlsSetting {
    /// Plain HTTP.
    Off,
    /// Auto-generated self-signed certificate — only works with executors
    /// that skip TLS verification.
    SelfSigned,
    /// User-supplied PEM cert + key. The route to a cert executors can
    /// VERIFY: a CA-signed cert for a domain resolving to this device
    /// (magic DNS like traefik.me / local-ip.co publish wildcard certs+keys,
    /// or use your own domain). No CA signs bare private-IP certs.
    Custom,
}

struct ServerTab {
    port: u16,
    bind_all: bool,
    handle: Option<jobs::ServerHandle>,
    state: ServerState,
    /// Cached LAN address for executor-facing URLs (refreshable in the UI).
    lan_ip: Option<std::net::IpAddr>,
    /// TLS mode for the next start.
    tls: TlsSetting,
    /// PEM paths for [`TlsSetting::Custom`].
    cert_path: String,
    key_path: String,
    /// Optional override for the host executors should use (e.g. a magic-DNS
    /// name like `192-168-1-7.my.local-ip.co` whose CA-signed cert is loaded
    /// in Custom mode). Empty = auto (LAN IP / localhost).
    advertised_host: String,
    /// In-flight local-ip.co certificate download.
    cert_fetch: Option<Receiver<Result<(PathBuf, PathBuf), String>>>,
    #[cfg(target_os = "android")]
    keep_screen_on: bool,
    /// Last time the UI probed 127.0.0.1:port for the service-hosted server.
    #[cfg(target_os = "android")]
    last_probe: Option<std::time::Instant>,
    /// When the start intent was fired (grace period before declaring failure).
    #[cfg(target_os = "android")]
    started_at: Option<std::time::Instant>,
    /// POST_NOTIFICATIONS has been requested once this session.
    #[cfg(target_os = "android")]
    notifications_requested: bool,
}

struct ScriptsTab {
    show_single: bool,
    show_batch: bool,
    last_saved: Option<String>,
}

// ---------------------------------------------------------------------------
// The application
// ---------------------------------------------------------------------------

pub struct TovekApp {
    tab: Tab,
    default_dir: PathBuf,
    zoom: f32,
    zoom_applied: f32,
    picker: Option<(PickTarget, Picker)>,
    single: SingleTab,
    folder: FolderTab,
    server: ServerTab,
    scripts: ScriptsTab,
    toast: Option<String>,
    #[cfg(target_os = "android")]
    android_app: Option<AndroidHandle>,
}

impl TovekApp {
    pub fn new(
        _cc: &eframe::CreationContext<'_>,
        data_dir: Option<PathBuf>,
        android: Option<AndroidHandle>,
    ) -> Self {
        #[cfg(not(target_os = "android"))]
        let _ = android;
        let default_dir = data_dir
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"));
        let zoom = if cfg!(target_os = "android") { 1.2 } else { 1.0 };
        Self {
            tab: Tab::Single,
            default_dir,
            zoom,
            zoom_applied: 1.0,
            picker: None,
            single: SingleTab {
                input_path: String::new(),
                force_raw: false,
                paste_mode: false,
                pasted: String::new(),
                script_name: String::new(),
                opts: OptionsState::for_single(),
                job: None,
                status: String::new(),
                output: String::new(),
                error: None,
            },
            folder: FolderTab {
                src: String::new(),
                out: String::new(),
                threads: 0,
                extension_lua: false,
                emit_upvalue_analysis: false,
                export_manifest: String::new(),
                cache_dir: String::new(),
                cache_max_mib: 512,
                opts: OptionsState::for_folder(),
                job: None,
                validating: false,
                analyze_path: String::new(),
                tool_dir: String::new(),
                old_solver: false,
                result: None,
            },
            server: ServerTab {
                port: 3000,
                // Android: executors can only reach the phone's LAN address
                // (http://<phone-ip>:<port>), never localhost — bind all
                // interfaces by default. Desktop keeps loopback-only.
                bind_all: cfg!(target_os = "android"),
                handle: None,
                // The Android server lives in the :tovek_server service
                // process and survives UI restarts: adopt it if it's up.
                #[cfg(target_os = "android")]
                state: if jobs::probe_port(3000) {
                    ServerState::Running(std::net::SocketAddr::from(([127, 0, 0, 1], 3000)))
                } else {
                    ServerState::Stopped
                },
                #[cfg(not(target_os = "android"))]
                state: ServerState::Stopped,
                lan_ip: jobs::lan_ip(),
                tls: TlsSetting::Off,
                cert_path: String::new(),
                key_path: String::new(),
                advertised_host: String::new(),
                cert_fetch: None,
                #[cfg(target_os = "android")]
                keep_screen_on: true,
                #[cfg(target_os = "android")]
                last_probe: None,
                #[cfg(target_os = "android")]
                started_at: None,
                #[cfg(target_os = "android")]
                notifications_requested: false,
            },
            scripts: ScriptsTab {
                show_single: false,
                show_batch: false,
                last_saved: None,
            },
            toast: None,
            #[cfg(target_os = "android")]
            android_app: android,
        }
    }

    fn open_picker(&mut self, target: PickTarget, kind: PickKind, title: &str, start_hint: &str) {
        let start = if start_hint.is_empty() {
            self.default_dir.clone()
        } else {
            PathBuf::from(start_hint)
        };
        self.picker = Some((target, Picker::new(kind, title, &start)));
    }

    fn apply_pick(&mut self, target: PickTarget, path: PathBuf) {
        let text = path.display().to_string();
        match target {
            PickTarget::SingleInput => self.single.input_path = text,
            PickTarget::SingleSaveDir => self.save_single_output(&path),
            PickTarget::FolderSrc => self.folder.src = text,
            PickTarget::FolderOut => self.folder.out = text,
            PickTarget::FolderManifest => self.folder.export_manifest = text,
            PickTarget::FolderCache => self.folder.cache_dir = text,
            PickTarget::ValidateAnalyze => self.folder.analyze_path = text,
            PickTarget::ValidateToolDir => self.folder.tool_dir = text,
            PickTarget::ScriptsDir => self.save_client_scripts(&path),
            PickTarget::TlsCert => self.server.cert_path = text,
            PickTarget::TlsKey => self.server.key_path = text,
        }
    }

    fn save_single_output(&mut self, dir: &Path) {
        let mut name = if !self.single.script_name.trim().is_empty() {
            sanitize_file_name(self.single.script_name.trim())
        } else {
            Path::new(&self.single.input_path)
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .filter(|stem| !stem.is_empty())
                .unwrap_or_else(|| "decompiled".to_owned())
        };
        name.push_str(".luau");
        let path = dir.join(name);
        match std::fs::write(&path, self.single.output.as_bytes()) {
            Ok(()) => self.toast = Some(format!("Saved {}", path.display())),
            Err(error) => self.toast = Some(format!("Save failed: {error}")),
        }
    }

    fn save_client_scripts(&mut self, dir: &Path) {
        let origin = self.executor_origin();
        let files = [
            (
                "decompile.client.luau",
                scripts::single_client_for_origin(&origin),
            ),
            (
                "decompile-batch.client.luau",
                scripts::batch_client_for_origin(&origin),
            ),
        ];
        let mut saved = Vec::new();
        for (name, body) in files {
            let path = dir.join(name);
            match std::fs::write(&path, body.as_bytes()) {
                Ok(()) => saved.push(path.display().to_string()),
                Err(error) => {
                    self.scripts.last_saved = Some(format!("Save failed: {error}"));
                    return;
                }
            }
        }
        self.scripts.last_saved = Some(format!("Saved {}", saved.join(" and ")));
    }

    fn poll_jobs(&mut self, ctx: &egui::Context) {
        let mut busy = false;

        if let Some(rx) = &self.single.job {
            match rx.try_recv() {
                Ok(outcome) => {
                    self.single.job = None;
                    match outcome.result {
                        Ok(source) => {
                            self.single.error = None;
                            self.single.status = format!(
                                "Decompiled in {:.2?} (key {})",
                                outcome.elapsed, outcome.used_key
                            );
                            self.single.output = source;
                        }
                        Err(error) => {
                            self.single.status = String::new();
                            self.single.output.clear();
                            self.single.error = Some(error);
                        }
                    }
                }
                Err(TryRecvError::Empty) => busy = true,
                Err(TryRecvError::Disconnected) => {
                    self.single.job = None;
                    self.single.error = Some("the decompile worker stopped unexpectedly".into());
                }
            }
        }

        if let Some(rx) = &self.folder.job {
            match rx.try_recv() {
                Ok(outcome) => {
                    self.folder.job = None;
                    let verb = if self.folder.validating { "Validate" } else { "Decompile" };
                    let ok = outcome.code == 0;
                    let message = format!(
                        "{verb} finished in {:.2?}: exit code {} ({} output file{})",
                        outcome.elapsed,
                        outcome.code,
                        outcome.outputs,
                        if outcome.outputs == 1 { "" } else { "s" },
                    );
                    self.folder.result = Some((message, ok));
                }
                Err(TryRecvError::Empty) => busy = true,
                Err(TryRecvError::Disconnected) => {
                    self.folder.job = None;
                    self.folder.result =
                        Some(("the folder worker stopped unexpectedly".into(), false));
                }
            }
        }

        // Drain server events first (so the borrow of the handle ends), then
        // apply them.
        let mut events = Vec::new();
        let mut disconnected = false;
        if let Some(handle) = &self.server.handle {
            loop {
                match handle.events.try_recv() {
                    Ok(event) => events.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        let mut drop_handle = false;
        for event in events {
            match event {
                ServerEvent::Running(addr) => {
                    self.server.state = ServerState::Running(addr);
                }
                ServerEvent::Stopped => {
                    self.server.state = ServerState::Stopped;
                    drop_handle = true;
                }
                ServerEvent::Failed(error) => {
                    self.server.state = ServerState::Failed(error);
                    drop_handle = true;
                }
            }
        }
        if disconnected {
            if matches!(self.server.state, ServerState::Starting) {
                self.server.state = ServerState::Failed("server thread exited".into());
            }
            drop_handle = true;
        }
        if drop_handle {
            self.server.handle = None;
        }
        if matches!(self.server.state, ServerState::Starting) {
            busy = true;
        }

        // local-ip.co certificate download finishing?
        if let Some(rx) = &self.server.cert_fetch {
            match rx.try_recv() {
                Ok(result) => {
                    self.server.cert_fetch = None;
                    match result {
                        Ok((cert, key)) => {
                            self.server.cert_path = cert.display().to_string();
                            self.server.key_path = key.display().to_string();
                            if let Some(host) =
                                self.server.lan_ip.and_then(jobs::local_ip_host)
                            {
                                self.server.advertised_host = host;
                            }
                            self.toast =
                                Some("local-ip.co certificate installed (re-fetch when it expires)".into());
                        }
                        Err(error) => self.toast = Some(format!("local-ip.co fetch failed: {error}")),
                    }
                }
                Err(TryRecvError::Empty) => busy = true,
                Err(TryRecvError::Disconnected) => {
                    self.server.cert_fetch = None;
                    self.toast = Some("local-ip.co fetch failed: worker stopped".into());
                }
            }
        }

        // Android: the server runs in the :tovek_server service process, so
        // the UI can only observe it from the outside — probe the port about
        // once a second while anything server-related is (or should be) up.
        #[cfg(target_os = "android")]
        {
            let due = self
                .server
                .last_probe
                .is_none_or(|t| t.elapsed() >= Duration::from_millis(1000));
            if due {
                self.server.last_probe = Some(std::time::Instant::now());
                let up = jobs::probe_port(self.server.port);
                self.server.state = match (&self.server.state, up) {
                    (_, true) => {
                        self.server.started_at = None;
                        ServerState::Running(std::net::SocketAddr::from((
                            [127, 0, 0, 1],
                            self.server.port,
                        )))
                    }
                    (ServerState::Starting, false) => {
                        if self.server.started_at.is_some_and(|t| t.elapsed() < Duration::from_secs(15)) {
                            ServerState::Starting
                        } else {
                            ServerState::Failed(
                                "the keep-alive service did not come up — check logcat".into(),
                            )
                        }
                    }
                    (ServerState::Failed(error), false) => ServerState::Failed(error.clone()),
                    (_, false) => ServerState::Stopped,
                };
            }
            ctx.request_repaint_after(Duration::from_millis(1100));
        }

        if busy {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// The `host:port` an executor must POST to: the advertised host when
    /// set (e.g. a magic-DNS name matching a verified cert), otherwise the
    /// LAN address when the server is reachable from other devices (always
    /// the case on Android, where executors cannot resolve the app's
    /// localhost), plain localhost otherwise.
    fn executor_host(&self) -> String {
        let advertised = self.server.advertised_host.trim();
        if !advertised.is_empty() {
            return if advertised.contains(':') {
                advertised.to_owned()
            } else {
                format!("{advertised}:{}", self.server.port)
            };
        }
        if self.server.bind_all
            && let Some(ip) = self.server.lan_ip
        {
            return format!("{ip}:{}", self.server.port);
        }
        format!("localhost:{}", self.server.port)
    }

    /// Full scheme + host executors must use (what the client scripts are
    /// retargeted at).
    fn executor_origin(&self) -> String {
        let scheme = if matches!(self.server.tls, TlsSetting::Off) {
            "http"
        } else {
            "https"
        };
        format!("{scheme}://{}", self.executor_host())
    }

    /// Resolve the PEM pair for the configured TLS mode (`None` = plain HTTP).
    fn tls_paths(&self) -> Result<Option<(PathBuf, PathBuf)>, String> {
        match self.server.tls {
            TlsSetting::Off => Ok(None),
            TlsSetting::SelfSigned => {
                let mut sans = vec!["localhost".to_owned(), "127.0.0.1".to_owned()];
                if let Some(ip) = self.server.lan_ip {
                    sans.push(ip.to_string());
                }
                let advertised = self.server.advertised_host.trim();
                if !advertised.is_empty() {
                    let host = advertised.split(':').next().unwrap_or(advertised);
                    sans.push(host.to_owned());
                }
                jobs::ensure_self_signed(&self.default_dir, &sans).map(Some)
            }
            TlsSetting::Custom => {
                let cert = self.server.cert_path.trim();
                let key = self.server.key_path.trim();
                if cert.is_empty() || key.is_empty() {
                    return Err("pick both the certificate and the private key (PEM)".into());
                }
                Ok(Some((PathBuf::from(cert), PathBuf::from(key))))
            }
        }
    }

    // -----------------------------------------------------------------------
    // Tab UIs
    // -----------------------------------------------------------------------

    fn single_tab_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Decompile one script");
        ui.add_space(4.0);

        ui.checkbox(&mut self.single.paste_mode, "Paste text instead of picking a file")
            .on_hover_text("Paste a base64 blob or a whole UniversalSynSaveInstance dump (-- comments + base64)");
        if self.single.paste_mode {
            egui::ScrollArea::vertical()
                .id_salt("paste-input")
                .max_height(140.0)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut self.single.pasted)
                            .code_editor()
                            .desired_rows(5)
                            .desired_width(f32::INFINITY)
                            .hint_text("base64 bytecode, or a full `--` dump file"),
                    );
                });
        } else {
            ui.horizontal(|ui| {
                ui.label("Input file:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.single.input_path)
                        .desired_width(ui.available_width() - 110.0)
                        .hint_text("saved-bytecode .lua / raw .luaubc"),
                );
                if ui.button("Browse…").clicked() {
                    let hint = self.single.input_path.clone();
                    self.open_picker(PickTarget::SingleInput, PickKind::File, "Pick a bytecode file", &hint);
                }
            });
            ui.checkbox(&mut self.single.force_raw, "Treat the file as raw bytecode")
                .on_hover_text("Skip the base64 text-dump detection for files that happen to look like text");
        }

        ui.horizontal(|ui| {
            ui.label("Script name:");
            ui.add(
                egui::TextEdit::singleline(&mut self.single.script_name)
                    .desired_width(300.0)
                    .hint_text("optional (--script-name), e.g. game.ReplicatedStorage.Module"),
            );
        });

        egui::CollapsingHeader::new("Decompile options")
            .default_open(true)
            .show(ui, |ui| {
                self.single.opts.key_ui(ui, true);
                self.single.opts.flags_ui(ui, false);
            });

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let ready = self.single.job.is_none()
                && (self.single.paste_mode && !self.single.pasted.trim().is_empty()
                    || !self.single.paste_mode && !self.single.input_path.trim().is_empty());
            if ui
                .add_enabled(ready, egui::Button::new("▶ Decompile"))
                .clicked()
            {
                self.start_single();
            }
            if self.single.job.is_some() {
                ui.spinner();
                ui.label("decompiling…");
            } else if !self.single.status.is_empty() {
                ui.weak(&self.single.status);
            }
        });

        if let Some(error) = &self.single.error {
            ui.colored_label(egui::Color32::LIGHT_RED, error);
        }

        if !self.single.output.is_empty() {
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(format!("Output ({} bytes)", self.single.output.len()));
                if ui.button("📋 Copy").clicked() {
                    ui.ctx().copy_text(self.single.output.clone());
                    self.toast = Some("Output copied to clipboard".into());
                }
                if ui.button("💾 Save to folder…").clicked() {
                    let hint = self.default_dir.display().to_string();
                    self.open_picker(
                        PickTarget::SingleSaveDir,
                        PickKind::Dir,
                        "Pick the output folder",
                        &hint,
                    );
                }
            });
            egui::ScrollArea::both()
                .id_salt("single-output")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut self.single.output.as_str())
                            .code_editor()
                            .desired_width(f32::INFINITY),
                    );
                });
        }
    }

    fn start_single(&mut self) {
        self.single.error = None;
        self.single.status = String::new();
        let bytecode = if self.single.paste_mode {
            jobs::extract_bytecode_from_text(&self.single.pasted)
        } else {
            jobs::load_bytecode_file(Path::new(self.single.input_path.trim()), self.single.force_raw)
                .map(|(bytes, kind)| {
                    self.single.status = format!(
                        "Loaded {} bytes ({})",
                        bytes.len(),
                        kind_describe(kind)
                    );
                    bytes
                })
        };
        match bytecode {
            Ok(bytes) => {
                let script_name = match self.single.script_name.trim() {
                    "" => None,
                    name => Some(name.to_owned()),
                };
                self.single.job = Some(jobs::spawn_single(
                    bytes,
                    self.single.opts.key_plan(),
                    script_name,
                    self.single.opts.to_options(),
                ));
            }
            Err(error) => self.single.error = Some(error),
        }
    }

    fn folder_tab_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Decompile a folder");
        ui.weak("The same driver as `luau-lifter decompile-folder`: every saved-bytecode .lua under SRC is decompiled in parallel and the tree is mirrored into OUT.");
        ui.add_space(4.0);

        {
            let hint = self.folder.src.clone();
            let mut clicked = false;
            ui.horizontal(|ui| {
                ui.label("Source folder:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.folder.src)
                        .desired_width(ui.available_width() - 110.0)
                        .hint_text("folder of saved-bytecode .lua files"),
                );
                clicked = ui.button("Browse…").clicked();
            });
            if clicked {
                self.open_picker(PickTarget::FolderSrc, PickKind::Dir, "Pick the source folder", &hint);
            }
        }
        {
            let hint = self.folder.out.clone();
            let mut clicked = false;
            ui.horizontal(|ui| {
                ui.label("Output folder:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.folder.out)
                        .desired_width(ui.available_width() - 110.0)
                        .hint_text("mirrored tree, .lua -> .luau"),
                );
                clicked = ui.button("Browse…").clicked();
            });
            if clicked {
                self.open_picker(PickTarget::FolderOut, PickKind::Dir, "Pick the output folder", &hint);
            }
        }

        ui.horizontal_wrapped(|ui| {
            ui.label("Threads:");
            ui.add(egui::DragValue::new(&mut self.folder.threads).speed(1))
                .on_hover_text("0 = one per logical CPU. The pool is created once per app run, so changes apply to the first folder run only");
            ui.separator();
            ui.label("Output extension:");
            ui.selectable_value(&mut self.folder.extension_lua, false, ".luau");
            ui.selectable_value(&mut self.folder.extension_lua, true, ".lua");
        });

        egui::CollapsingHeader::new("Decompile options")
            .default_open(true)
            .show(ui, |ui| {
                self.folder.opts.key_ui(ui, false);
                self.folder.opts.flags_ui(ui, true);
                ui.checkbox(
                    &mut self.folder.emit_upvalue_analysis,
                    "Emit upvalue analysis (--emit-upvalue-analysis)",
                )
                .on_hover_text("Immutable static upvalue metadata under OUT/.tovek-analysis");
            });

        egui::CollapsingHeader::new("Advanced (manifest + cache)").show(ui, |ui| {
            {
                let hint = self.folder.export_manifest.clone();
                let mut clicked = false;
                ui.horizontal(|ui| {
                    ui.label("Export manifest:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.folder.export_manifest)
                            .desired_width(ui.available_width() - 150.0)
                            .hint_text("optional Volt exporter manifest (--export-manifest)"),
                    );
                    clicked = ui.button("Browse…").clicked();
                    if ui.button("✖").clicked() {
                        self.folder.export_manifest.clear();
                    }
                });
                if clicked {
                    self.open_picker(PickTarget::FolderManifest, PickKind::File, "Pick the export manifest", &hint);
                }
            }
            {
                let hint = self.folder.cache_dir.clone();
                let mut clicked = false;
                ui.horizontal(|ui| {
                    ui.label("Cache folder:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.folder.cache_dir)
                            .desired_width(ui.available_width() - 150.0)
                            .hint_text("optional verified artifact cache (--cache-dir)"),
                    );
                    clicked = ui.button("Browse…").clicked();
                    if ui.button("✖").clicked() {
                        self.folder.cache_dir.clear();
                    }
                });
                if clicked {
                    self.open_picker(PickTarget::FolderCache, PickKind::Dir, "Pick the cache folder", &hint);
                }
            }
            ui.horizontal(|ui| {
                ui.label("Cache limit (MiB):");
                ui.add(egui::DragValue::new(&mut self.folder.cache_max_mib).speed(8));
            });
        });

        egui::CollapsingHeader::new("Validate with luau-analyze").show(ui, |ui| {
            ui.weak("The `validate-folder` driver: decompile, then gate every output through Luau's own parser. Needs a luau-analyze executable (desktop).");
            {
                let hint = self.folder.analyze_path.clone();
                let mut clicked = false;
                ui.horizontal(|ui| {
                    ui.label("luau-analyze:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.folder.analyze_path)
                            .desired_width(ui.available_width() - 110.0)
                            .hint_text("path to luau-analyze[.exe] (--analyze)"),
                    );
                    clicked = ui.button("Browse…").clicked();
                });
                if clicked {
                    self.open_picker(PickTarget::ValidateAnalyze, PickKind::File, "Pick luau-analyze", &hint);
                }
            }
            {
                let hint = self.folder.tool_dir.clone();
                let mut clicked = false;
                ui.horizontal(|ui| {
                    ui.label("Tool folder:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.folder.tool_dir)
                            .desired_width(ui.available_width() - 110.0)
                            .hint_text("folder holding luau-analyze (--tool-dir, used if the path above is unset)"),
                    );
                    clicked = ui.button("Browse…").clicked();
                });
                if clicked {
                    self.open_picker(PickTarget::ValidateToolDir, PickKind::Dir, "Pick the tool folder", &hint);
                }
            }
            ui.horizontal_wrapped(|ui| {
                ui.label("Typechecker:");
                ui.selectable_value(&mut self.folder.old_solver, false, "new (parity baseline)");
                ui.selectable_value(&mut self.folder.old_solver, true, "old (~8x faster)");
            });
            let ready = self.folder.job.is_none()
                && !self.folder.src.trim().is_empty()
                && !self.folder.out.trim().is_empty();
            if ui
                .add_enabled(ready, egui::Button::new("▶ Decompile + validate"))
                .clicked()
            {
                self.start_folder(true);
            }
        });

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let ready = self.folder.job.is_none()
                && !self.folder.src.trim().is_empty()
                && !self.folder.out.trim().is_empty();
            if ui
                .add_enabled(ready, egui::Button::new("▶ Decompile folder"))
                .clicked()
            {
                self.start_folder(false);
            }
            if self.folder.job.is_some() {
                ui.spinner();
                ui.label(if self.folder.validating {
                    "validating…"
                } else {
                    "decompiling…"
                });
            }
        });
        if let Some((message, ok)) = &self.folder.result {
            let color = if *ok {
                egui::Color32::LIGHT_GREEN
            } else {
                egui::Color32::LIGHT_RED
            };
            ui.colored_label(color, message);
            ui.weak("Detailed per-file errors and summaries print to the console when the app is launched from a terminal; failed scripts are also written as comments in the output tree.");
        }
    }

    fn start_folder(&mut self, validate: bool) {
        self.folder.result = None;
        self.folder.validating = validate;
        let src = PathBuf::from(self.folder.src.trim());
        let out = PathBuf::from(self.folder.out.trim());
        let options = self.folder.opts.to_options();
        let key = self.folder.opts.fixed_key();
        if validate {
            let analyze = non_empty_path(&self.folder.analyze_path);
            let tool_dir = non_empty_path(&self.folder.tool_dir);
            self.folder.job = Some(jobs::spawn_validate(jobs::ValidateParams {
                src,
                out,
                key,
                threads: self.folder.threads,
                options,
                analyze,
                tool_dir,
                old_solver: self.folder.old_solver,
            }));
        } else {
            self.folder.job = Some(jobs::spawn_folder(jobs::FolderParams {
                src,
                out,
                key,
                threads: self.folder.threads,
                options,
                emit_upvalue_analysis: self.folder.emit_upvalue_analysis
                    || options.emit_binding_provenance,
                output_extension: if self.folder.extension_lua { "lua" } else { "luau" }.to_owned(),
                export_manifest: non_empty_path(&self.folder.export_manifest),
                cache_dir: non_empty_path(&self.folder.cache_dir),
                cache_max_mib: self.folder.cache_max_mib,
            }));
        }
    }

    fn server_tab_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Decompiler server");
        ui.weak("The same HTTP server as the standalone `web-server` binary — point an executor client script at it and dump a whole game.");
        ui.add_space(4.0);

        #[cfg(target_os = "android")]
        let running = matches!(
            self.server.state,
            ServerState::Running(_) | ServerState::Starting
        );
        #[cfg(not(target_os = "android"))]
        let running = self.server.handle.is_some();
        ui.horizontal_wrapped(|ui| {
            ui.label("Port:");
            ui.add_enabled(
                !running,
                egui::DragValue::new(&mut self.server.port).speed(1).range(1..=65535),
            );
            if ui.add_enabled(!running, egui::Button::new("3000")).clicked() {
                self.server.port = 3000;
            }
            ui.separator();
            ui.add_enabled(
                !running,
                egui::Checkbox::new(&mut self.server.bind_all, "Listen on all interfaces (LAN)"),
            )
            .on_hover_text("Bind 0.0.0.0 so another device can reach the server — required on Android, where executors must use the phone's LAN address, never localhost. Only use on trusted networks.");
        });

        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            ui.label("HTTPS:");
            ui.add_enabled_ui(!running, |ui| {
                ui.selectable_value(&mut self.server.tls, TlsSetting::Off, "Off");
                ui.selectable_value(&mut self.server.tls, TlsSetting::SelfSigned, "Self-signed")
                    .on_hover_text("Auto-generated certificate. Works only with executors that skip TLS verification.");
                ui.selectable_value(&mut self.server.tls, TlsSetting::Custom, "Custom cert")
                    .on_hover_text("Your own PEM certificate + key — the way to a cert executors can actually verify.");
            });
        });
        match self.server.tls {
            TlsSetting::SelfSigned => {
                ui.horizontal_wrapped(|ui| {
                    ui.weak("Self-signed: executors must have TLS verification disabled.");
                    if ui.small_button("Regenerate").on_hover_text("Delete the stored certificate so the next start makes a fresh one (do this after the LAN address changes)").clicked() {
                        jobs::forget_self_signed(&self.default_dir);
                        self.toast = Some("Certificate will be regenerated on next start".into());
                    }
                });
            }
            TlsSetting::Custom => {
                ui.horizontal_wrapped(|ui| {
                    let fetching = self.server.cert_fetch.is_some();
                    if ui
                        .add_enabled(!fetching, egui::Button::new("⬇ Get local-ip.co cert"))
                        .on_hover_text("Downloads local-ip.co's CA-signed wildcard certificate + key (they publish both), builds the fullchain PEM, and fills everything in — including the <your-ip>.my.local-ip.co hostname. Re-fetch when the cert expires (~90 days).")
                        .clicked()
                    {
                        self.server.lan_ip = jobs::lan_ip();
                        let dir = self.default_dir.clone();
                        let (tx, rx) = std::sync::mpsc::channel();
                        std::thread::spawn(move || {
                            let _ = tx.send(jobs::fetch_local_ip_cert(&dir));
                        });
                        self.server.cert_fetch = Some(rx);
                    }
                    if fetching {
                        ui.spinner();
                        ui.weak("downloading…");
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("Cert (PEM):");
                    ui.add(egui::TextEdit::singleline(&mut self.server.cert_path).desired_width(260.0));
                    if ui.button("📂").clicked() {
                        let hint = self.default_dir.display().to_string();
                        self.open_picker(PickTarget::TlsCert, PickKind::File, "Pick the certificate (fullchain PEM)", &hint);
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("Key (PEM):");
                    ui.add(egui::TextEdit::singleline(&mut self.server.key_path).desired_width(260.0));
                    if ui.button("📂").clicked() {
                        let hint = self.default_dir.display().to_string();
                        self.open_picker(PickTarget::TlsKey, PickKind::File, "Pick the private key (PEM)", &hint);
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    ui.label("Advertised host:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.server.advertised_host)
                            .hint_text("e.g. 192-168-1-7.traefik.me")
                            .desired_width(260.0),
                    )
                    .on_hover_text("The hostname the certificate was issued for; scripts and URLs use it instead of the raw IP.");
                });
                ui.weak(
                    "For a VERIFIED cert on a LAN device: no CA signs private-IP certs, so use a \
                     magic-DNS domain that resolves to this device's IP with its published \
                     CA-signed wildcard cert. The button above sets up local-ip.co \
                     automatically; traefik.me or your own domain's cert work the same way \
                     via the pickers.",
                );
            }
            TlsSetting::Off => {}
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if running {
                if ui.button("⏹ Stop server").clicked() {
                    #[cfg(target_os = "android")]
                    {
                        match crate::android::stop_server_service() {
                            Ok(()) => self.server.state = ServerState::Stopped,
                            Err(error) => self.toast = Some(format!("stop service: {error}")),
                        }
                        self.server.started_at = None;
                        self.server.last_probe = None;
                    }
                    #[cfg(not(target_os = "android"))]
                    if let Some(handle) = &mut self.server.handle {
                        handle.stop();
                    }
                }
            } else if ui.button("▶ Start server").clicked() {
                self.server.lan_ip = jobs::lan_ip();
                match self.tls_paths() {
                    Err(error) => {
                        self.server.state = ServerState::Failed(error);
                    }
                    Ok(tls) => {
                        self.server.state = ServerState::Starting;
                        #[cfg(target_os = "android")]
                        {
                            if !self.server.notifications_requested {
                                self.server.notifications_requested = true;
                                if let Err(error) = crate::android::request_post_notifications() {
                                    log::warn!("POST_NOTIFICATIONS request: {error}");
                                }
                            }
                            self.server.started_at = Some(std::time::Instant::now());
                            self.server.last_probe = None;
                            let tls_strings = tls.as_ref().map(|(cert, key)| {
                                (cert.display().to_string(), key.display().to_string())
                            });
                            if let Err(error) = crate::android::start_server_service(
                                self.server.port,
                                self.server.bind_all,
                                tls_strings
                                    .as_ref()
                                    .map(|(cert, key)| (cert.as_str(), key.as_str())),
                            ) {
                                self.server.state = ServerState::Failed(error);
                            }
                        }
                        #[cfg(not(target_os = "android"))]
                        {
                            self.server.handle = Some(jobs::start_server(
                                self.server.port,
                                self.server.bind_all,
                                tls,
                            ));
                        }
                    }
                }
            }
            match &self.server.state {
                ServerState::Stopped => ui.weak("stopped"),
                ServerState::Starting => {
                    ui.spinner();
                    ui.weak("starting…")
                }
                ServerState::Running(addr) => ui.colored_label(
                    egui::Color32::LIGHT_GREEN,
                    format!("listening on http://{addr}"),
                ),
                ServerState::Failed(error) => {
                    ui.colored_label(egui::Color32::LIGHT_RED, format!("failed: {error}"))
                }
            };
        });

        let origin = self.executor_origin();
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.label("Executor URL:");
            ui.monospace(origin.clone());
            if ui
                .small_button("↻")
                .on_hover_text("Re-detect this device's LAN address")
                .clicked()
            {
                self.server.lan_ip = jobs::lan_ip();
            }
        });
        if self.server.bind_all && self.server.lan_ip.is_none() {
            ui.colored_label(
                egui::Color32::LIGHT_RED,
                "No LAN address detected — is WiFi connected?",
            );
        }
        ui.label("Routes (identical to the standalone server):");
        ui.monospace(format!(
            "POST {origin}/decompile        one script, base64 body\nPOST {origin}/decompile/raw    one script, raw bytecode body\nPOST {origin}/decompile/batch  many scripts, JSON or MDB1",
        ));
        ui.weak("Per-request options: X-Encode-Key header, and X-Flags (names like `dont_reuse_var,strict_no_synthetic_control` or the numeric bit mask) — every decompile option is reachable over HTTP too.");

        #[cfg(target_os = "android")]
        {
            ui.separator();
            ui.label("Foreground keep-alive");
            if ui
                .checkbox(&mut self.server.keep_screen_on, "Keep the screen on")
                .changed()
            {
                if let Some(app) = &self.android_app {
                    crate::android::keep_screen_on(app, self.server.keep_screen_on);
                }
            }
            ui.weak(
                "The server runs in its own foreground service (persistent notification, \
                 wake + WiFi locks) — it keeps listening even after this app is swiped away, \
                 like Termux. Stop it with the button above or the notification's Stop action.",
            );
        }

        ui.separator();
        ui.label(format!(
            "Executor client scripts (host scripts), retargeted at {origin}:"
        ));
        ui.horizontal_wrapped(|ui| {
            if ui.button("📋 Copy single-script client").clicked() {
                ui.ctx()
                    .copy_text(scripts::single_client_for_origin(&origin));
                self.toast = Some("decompile.client.luau copied".into());
            }
            if ui.button("📋 Copy batch client").clicked() {
                ui.ctx()
                    .copy_text(scripts::batch_client_for_origin(&origin));
                self.toast = Some("decompile-batch.client.luau copied".into());
            }
            if ui.button("💾 Save both…").clicked() {
                let hint = self.default_dir.display().to_string();
                self.open_picker(PickTarget::ScriptsDir, PickKind::Dir, "Pick the folder for the client scripts", &hint);
            }
        });
        if let Some(message) = &self.scripts.last_saved {
            ui.weak(message);
        }
    }

    fn scripts_tab_ui(&mut self, ui: &mut egui::Ui) {
        let origin = self.executor_origin();
        ui.heading("Luau host scripts");
        ui.weak("The executor-side clients that drive the server — one for each decompile transport. They are embedded in this app (and shipped next to every release binary), already retargeted at the address the executor must use.");
        ui.add_space(4.0);
        ui.label(format!("Current server address: {origin}"));

        for (title, description, body, show) in [
            (
                "decompile.client.luau — single-script transport",
                "Installs getgenv().decompile and POSTs each script's bytecode (base64) to /decompile while SynSaveInstance walks the game.",
                scripts::single_client_for_origin(&origin),
                &mut self.scripts.show_single,
            ),
            (
                "decompile-batch.client.luau — batch + raw transport",
                "Pre-walks every script, ships whole chunks to /decompile/batch (binary MDB1 framing or base64 JSON), answers SynSaveInstance from the cache, falls back to single POSTs on misses.",
                scripts::batch_client_for_origin(&origin),
                &mut self.scripts.show_batch,
            ),
        ] {
            ui.separator();
            ui.label(egui::RichText::new(title).strong());
            ui.weak(description);
            ui.horizontal(|ui| {
                if ui.button("📋 Copy").clicked() {
                    ui.ctx().copy_text(body.clone());
                }
                ui.checkbox(&mut *show, "Preview");
            });
            if *show {
                egui::ScrollArea::vertical()
                    .id_salt(title)
                    .max_height(220.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut body.as_str())
                                .code_editor()
                                .desired_width(f32::INFINITY),
                        );
                    });
            }
        }

        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("💾 Save both to folder…").clicked() {
                let hint = self.default_dir.display().to_string();
                self.open_picker(
                    PickTarget::ScriptsDir,
                    PickKind::Dir,
                    "Pick the folder for the client scripts",
                    &hint,
                );
            }
            if let Some(message) = &self.scripts.last_saved {
                ui.weak(message);
            }
        });
    }

    fn about_tab_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading(format!("Tovek {VERSION}"));
        ui.label("A high-readability, high-performance Luau decompiler.");
        ui.add_space(6.0);
        ui.hyperlink_to("GitHub repository", "https://github.com/Kiet1308/Tovek");
        ui.hyperlink_to("Release notes", "https://kiet1308.github.io/Tovek/changelog.html");
        ui.add_space(6.0);
        ui.label("This GUI bundles every decompile surface of the project:");
        ui.label("• single-file (all CLI flags), • folder (the CLI-identical decompile-folder and validate-folder drivers), • the embedded HTTP server (same routes as web-server), • the executor client scripts for each transport.");
        ui.add_space(6.0);
        if cfg!(target_os = "android") {
            ui.label("Android build: NativeActivity (xbuild). While the server runs the app keeps the screen on and holds a partial wake lock; keep it in the foreground (or split-screen) while dumping.");
        }
        ui.weak("AI features are disabled: the decompiler uses deterministic rules and never calls external services.");
    }
}

impl eframe::App for TovekApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if (self.zoom - self.zoom_applied).abs() > f32::EPSILON {
            ctx.set_zoom_factor(self.zoom);
            self.zoom_applied = self.zoom;
        }

        self.poll_jobs(ctx);

        // Keep clear of the Android status bar / cutout.
        #[cfg(target_os = "android")]
        egui::TopBottomPanel::top("safe-area")
            .show_separator_line(false)
            .exact_height(24.0)
            .show(ctx, |_| {});

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("Tovek").strong().size(18.0));
                ui.separator();
                ui.selectable_value(&mut self.tab, Tab::Single, "Single file");
                ui.selectable_value(&mut self.tab, Tab::Folder, "Folder");
                ui.selectable_value(&mut self.tab, Tab::Server, "Server");
                ui.selectable_value(&mut self.tab, Tab::Scripts, "Client scripts");
                ui.selectable_value(&mut self.tab, Tab::About, "About");
                ui.separator();
                if ui.button("A−").on_hover_text("Smaller UI").clicked() {
                    self.zoom = (self.zoom - 0.1).max(0.7);
                }
                if ui.button("A+").on_hover_text("Larger UI").clicked() {
                    self.zoom = (self.zoom + 0.1).min(2.5);
                }
                #[cfg(target_os = "android")]
                if let Some(app) = &self.android_app {
                    if ui
                        .button("⌨")
                        .on_hover_text("Toggle the soft keyboard")
                        .clicked()
                    {
                        crate::android::show_soft_input(app, true);
                    }
                }
            });
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                match &self.server.state {
                    ServerState::Running(_) => {
                        ui.colored_label(
                            egui::Color32::LIGHT_GREEN,
                            format!("● server {}", self.executor_origin()),
                        );
                    }
                    ServerState::Starting => {
                        ui.weak("● server starting…");
                    }
                    _ => {
                        ui.weak("● server stopped");
                    }
                }
                if self.single.job.is_some() || self.folder.job.is_some() {
                    ui.separator();
                    ui.spinner();
                    ui.weak("working…");
                }
                if let Some(toast) = self.toast.clone() {
                    ui.separator();
                    ui.weak(toast);
                    if ui.small_button("✖").clicked() {
                        self.toast = None;
                    }
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("tab-scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| match self.tab {
                    Tab::Single => self.single_tab_ui(ui),
                    Tab::Folder => self.folder_tab_ui(ui),
                    Tab::Server => self.server_tab_ui(ui),
                    Tab::Scripts => self.scripts_tab_ui(ui),
                    Tab::About => self.about_tab_ui(ui),
                });
        });

        // The picker window floats above everything.
        if let Some((target, picker)) = &mut self.picker {
            let keep_open = picker.ui(ctx);
            if !keep_open {
                let target = *target;
                let picked = picker.picked.take();
                self.picker = None;
                if let Some(path) = picked {
                    self.apply_pick(target, path);
                }
            }
        }

        // eframe's clipboard backend (arboard) has no Android support — every
        // egui copy command would silently vanish. Intercept them here and
        // route through the Java ClipboardManager instead. This covers every
        // 📋 button in the app.
        #[cfg(target_os = "android")]
        {
            let mut copied: Option<String> = None;
            ctx.output_mut(|out| {
                out.commands.retain(|command| match command {
                    egui::OutputCommand::CopyText(text) => {
                        copied = Some(text.clone());
                        false
                    }
                    _ => true,
                });
            });
            if let Some(text) = copied {
                match crate::android::copy_to_clipboard(&text) {
                    Ok(()) => self.toast = Some("Copied to clipboard".into()),
                    Err(error) => self.toast = Some(format!("copy failed: {error}")),
                }
            }
        }
    }
}

fn kind_describe(kind: InputKind) -> &'static str {
    kind.describe()
}

fn non_empty_path(text: &str) -> Option<PathBuf> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

fn sanitize_file_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '.',
            other => other,
        })
        .collect()
}

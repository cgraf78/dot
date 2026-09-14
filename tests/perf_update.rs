//! Release-mode performance gate for the native `dot` port.
//!
//! The ignored test compares the final reachable Bash implementation with the
//! current native binary on separate clients backed by the same local remotes.
//! Setup and semantic validation stay outside timed regions. Measured pairs
//! alternate engine order, every dirty pair receives a distinct upstream
//! commit, and a sample is recorded only after both engines have converged to
//! equivalent normalized output and filesystem state.
//!
//! Run this only through `scripts/benchmark-port.sh`: the driver constructs the
//! immutable shell checkout, stamps the native build identity, forces Cargo's
//! release profile, and gives Rust a run-private provisional evidence directory.
//! The driver validates and publishes that evidence only after this test exits.

#[path = "support/perf_policy.rs"]
mod perf_policy;

#[cfg(target_os = "linux")]
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
#[cfg(target_os = "linux")]
use std::io::Read as _;
use std::io::{BufWriter, Write as _};
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::PermissionsExt as _;
#[cfg(target_os = "linux")]
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
#[cfg(target_os = "linux")]
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

use perf_policy::{
    EngineKind, REQUIRED_WORKLOADS, RUNS, STARTUP_PREFLIGHT_PAIRS, STARTUP_WARMUPS, Stats,
    UPDATE_WARMUPS, WORKLOAD_POLICIES, Workload, budget_headroom_percent, meets_relative_gate,
    pair_order, ratio_percent_ceil, shell_baseline_sha, startup_warmup_schedule, summarize,
};

const OVERLAYS: usize = 3;
const FILES_PER_OVERLAY: usize = 20;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const CAPTURE_LIMIT_BYTES: u64 = 1024 * 1024;
#[cfg(target_os = "linux")]
const TERMINATION_GRACE: Duration = Duration::from_millis(250);
#[cfg(target_os = "linux")]
const FORCED_CLEANUP_GRACE: Duration = Duration::from_secs(1);
#[cfg(target_os = "linux")]
const SUCCESS_QUIESCENCE_GRACE: Duration = Duration::from_millis(500);
#[cfg(target_os = "linux")]
const PROCESS_POLL: Duration = Duration::from_millis(10);
#[cfg(target_os = "linux")]
const MAX_SUPERVISOR_RECORD_BYTES: usize = 64 * 1024;
#[cfg(target_os = "linux")]
const MAX_SUPERVISOR_RECORD_FILE_BYTES: u64 = 512 * 1024;
const ARTIFACT_DIR_ENV: &str = "DOT_PERF_ARTIFACT_DIR";
const CURRENT_SHA_ENV: &str = "DOT_PERF_CURRENT_SHA";
const SHELL_ROOT_ENV: &str = "DOT_PERF_SHELL_ROOT";
const GIT_ENV: &str = "DOT_PERF_GIT";
const BASH_ENV: &str = "DOT_PERF_BASH";
const CARGO_ENV: &str = "DOT_PERF_CARGO";
const RUSTC_ENV: &str = "DOT_PERF_RUSTC";
const CLIENT_PATH_ENV: &str = "DOT_PERF_CLIENT_PATH";
const SHDEPS_ROOT_ENV: &str = "DOT_PERF_SHDEPS_ROOT";
const SHDEPS_BINARY_ENV: &str = "DOT_PERF_SHDEPS_BINARY";
const RUNNER_IMAGE_ENV: &str = "DOT_PERF_RUNNER_IMAGE";
const RUN_ID_ENV: &str = "DOT_PERF_RUN_ID";
#[cfg(target_os = "linux")]
const SUPERVISOR_SPEC_ENV: &str = "DOT_PERF_SUPERVISOR_SPEC";
#[cfg(target_os = "linux")]
const SUPERVISOR_SCAN_FAULT_ENV: &str = "DOT_PERF_TEST_SUPERVISOR_SCAN_FAULT";
#[cfg(target_os = "linux")]
const SUPERVISOR_DIRECT_PID_FILE_ENV: &str = "DOT_PERF_TEST_SUPERVISOR_DIRECT_PID_FILE";

type Scratch = dot_test_support::TempDir;

#[derive(Debug)]
struct PerfTools {
    git: PathBuf,
    bash: PathBuf,
    cargo: PathBuf,
    rustc: PathBuf,
    client_path: OsString,
}

impl PerfTools {
    fn from_env() -> Result<Self, String> {
        let tools = Self {
            git: required_tool(GIT_ENV)?,
            bash: required_tool(BASH_ENV)?,
            cargo: required_tool(CARGO_ENV)?,
            rustc: required_tool(RUSTC_ENV)?,
            client_path: std::env::var_os(CLIENT_PATH_ENV)
                .ok_or_else(|| format!("{CLIENT_PATH_ENV} is required"))?,
        };
        let path_git = resolve_in_path(OsStr::new("git"), &tools.client_path)
            .ok_or_else(|| "controlled client PATH does not contain Git".to_string())?;
        if path_git != tools.git {
            return Err(format!(
                "controlled client PATH resolves Git to {}, expected {}",
                path_git.display(),
                tools.git.display()
            ));
        }
        Ok(tools)
    }

    #[cfg(test)]
    fn system() -> Result<Self, String> {
        let git = system_tool("git")?;
        let bash = system_tool("bash")?;
        let search_path = std::env::var_os("PATH").unwrap_or_default();
        let cargo = discover_build_tool("cargo", &search_path).or_else(|_| system_tool("cargo"))?;
        let rustc = discover_build_tool("rustc", &search_path).or_else(|_| system_tool("rustc"))?;
        let directories = [&git, &bash]
            .into_iter()
            .filter_map(|path| path.parent())
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        let client_path = std::env::join_paths(directories)
            .map_err(|error| format!("construct controlled PATH: {error}"))?;
        Ok(Self {
            git,
            bash,
            cargo,
            rustc,
            client_path,
        })
    }
}

fn required_tool(name: &str) -> Result<PathBuf, String> {
    let value = std::env::var_os(name).ok_or_else(|| format!("{name} is required"))?;
    canonical_executable(Path::new(&value)).map_err(|error| format!("{name}: {error}"))
}

fn canonical_executable(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("tool path is not absolute: {}", path.display()));
    }
    let name = path
        .file_name()
        .ok_or_else(|| format!("tool path has no file name: {}", path.display()))?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("tool path has no parent directory: {}", path.display()))?;
    // Normalize the containing directory but keep the final component
    // unresolved: argv[0]-dispatched proxies (rustup shims) must keep their
    // selected name, so resolving `cargo` to the `rustup` manager binary
    // would execute the tool under the wrong identity.
    let canonical = fs::canonicalize(parent)
        .map_err(|error| format!("canonicalize {}: {error}", parent.display()))?
        .join(name);
    let metadata = fs::metadata(&canonical)
        .map_err(|error| format!("inspect {}: {error}", canonical.display()))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(format!(
            "tool is not an executable file: {}",
            canonical.display()
        ));
    }
    Ok(canonical)
}

#[cfg(test)]
fn system_tool(name: &str) -> Result<PathBuf, String> {
    for directory in [
        "/usr/bin",
        "/bin",
        "/usr/local/bin",
        "/opt/homebrew/bin",
        "/opt/local/bin",
    ] {
        let candidate = Path::new(directory).join(name);
        if let Ok(path) = canonical_executable(&candidate) {
            return Ok(path);
        }
    }
    Err(format!("no system {name} found"))
}

#[cfg(test)]
fn discover_build_tool(name: &str, path: &OsStr) -> Result<PathBuf, String> {
    resolve_in_path(OsStr::new(name), path)
        .ok_or_else(|| format!("no executable {name} in the build-tool PATH"))
}

fn resolve_in_path(name: &OsStr, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path).find_map(|directory| {
        let candidate = directory.join(name);
        canonical_executable(&candidate).ok()
    })
}

#[derive(Debug)]
struct Engine {
    kind: EngineKind,
    executable: PathBuf,
    source_root: PathBuf,
    commit: String,
}

impl Engine {
    fn short_commit(&self) -> &str {
        &self.commit[..12]
    }

    fn version_line(&self) -> Vec<u8> {
        format!(
            "dot commit {} (config 1; extensions 1; library 1)\n",
            self.short_commit()
        )
        .into_bytes()
    }
}

#[derive(Debug)]
struct Engines {
    shell: Engine,
    rust: Engine,
}

#[derive(Debug)]
struct ProviderIdentity {
    current_revision: String,
    shell_revision: String,
    abi: String,
}

#[derive(Debug)]
struct EvidenceIdentity {
    run_id: String,
    shell_commit: String,
    shell_tree: String,
    rust_commit: String,
    rust_tree: String,
    shdeps_commit: String,
    shdeps_tree: String,
    git_blob: String,
    bash_blob: String,
    cargo_blob: String,
    rustc_blob: String,
    shell_executable_blob: String,
    rust_executable_blob: String,
    shdeps_executable_blob: String,
}

impl EvidenceIdentity {
    fn new(
        run_id: &str,
        engines: &Engines,
        tools: &PerfTools,
        provider_root: &Path,
        provider_binary: &Path,
        provider_identity: &ProviderIdentity,
    ) -> Self {
        Self {
            run_id: run_id.to_string(),
            shell_commit: engines.shell.commit.clone(),
            shell_tree: git_text(
                tools,
                &engines.shell.source_root,
                &["rev-parse", "HEAD^{tree}"],
            ),
            rust_commit: engines.rust.commit.clone(),
            rust_tree: git_text(
                tools,
                &engines.rust.source_root,
                &["rev-parse", "HEAD^{tree}"],
            ),
            shdeps_commit: provider_identity.current_revision.clone(),
            shdeps_tree: git_text(tools, provider_root, &["rev-parse", "HEAD^{tree}"]),
            git_blob: file_blob(tools, &tools.git),
            bash_blob: file_blob(tools, &tools.bash),
            cargo_blob: file_blob(tools, &tools.cargo),
            rustc_blob: file_blob(tools, &tools.rustc),
            shell_executable_blob: file_blob(tools, &engines.shell.executable),
            rust_executable_blob: file_blob(tools, &engines.rust.executable),
            shdeps_executable_blob: file_blob(tools, provider_binary),
        }
    }
}

impl Engines {
    fn get(&self, kind: EngineKind) -> &Engine {
        match kind {
            EngineKind::Shell => &self.shell,
            EngineKind::Rust => &self.rust,
        }
    }
}

#[derive(Debug)]
struct Client {
    home: PathBuf,
    state: PathBuf,
    xdg: PathBuf,
    data: PathBuf,
    cache: PathBuf,
    tmp: PathBuf,
    extra_env: Vec<(OsString, OsString)>,
}

#[derive(Debug)]
struct Clients {
    shell: Client,
    rust: Client,
}

impl Clients {
    fn get(&self, kind: EngineKind) -> &Client {
        match kind {
            EngineKind::Shell => &self.shell,
            EngineKind::Rust => &self.rust,
        }
    }
}

#[derive(Debug)]
struct TimedOutput {
    elapsed_ns: u128,
    output: Output,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct TreeEntry {
    path: Vec<u8>,
    kind: u8,
    mode: u32,
    value: Vec<u8>,
}

type ClientSnapshot = [Vec<TreeEntry>; 6];
type ImmutableInputs = [Vec<TreeEntry>; 3];

#[derive(Clone, Copy)]
enum ClientRoot {
    Home,
    State,
    Config,
    Data,
    Cache,
    Temporary,
}

impl ClientRoot {
    const ALL: [Self; 6] = [
        Self::Home,
        Self::State,
        Self::Config,
        Self::Data,
        Self::Cache,
        Self::Temporary,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Home => "HOME",
            Self::State => "XDG_STATE_HOME",
            Self::Config => "XDG_CONFIG_HOME",
            Self::Data => "XDG_DATA_HOME",
            Self::Cache => "XDG_CACHE_HOME",
            Self::Temporary => "TMPDIR",
        }
    }

    fn path(self, client: &Client) -> &Path {
        match self {
            Self::Home => &client.home,
            Self::State => &client.state,
            Self::Config => &client.xdg,
            Self::Data => &client.data,
            Self::Cache => &client.cache,
            Self::Temporary => &client.tmp,
        }
    }
}

#[derive(Debug, Default)]
struct Measurements {
    shell: Vec<u128>,
    rust: Vec<u128>,
}

impl Measurements {
    fn push(&mut self, kind: EngineKind, elapsed_ns: u128) {
        match kind {
            EngineKind::Shell => self.shell.push(elapsed_ns),
            EngineKind::Rust => self.rust.push(elapsed_ns),
        }
    }

    fn stats(&self) -> (Stats, Stats) {
        (
            summarize(&self.shell).expect("shell samples"),
            summarize(&self.rust).expect("rust samples"),
        )
    }
}

#[derive(Debug)]
struct SummaryRow {
    workload: Workload,
    samples: usize,
    shell: Option<Stats>,
    rust: Stats,
    max_rust_percent: Option<u128>,
    rust_p95_budget_ns: u128,
    passed: bool,
}

struct MetadataContext<'a> {
    fixture_root: &'a Path,
    dirty: &'a str,
    runner_image: &'a str,
}

#[derive(Debug)]
struct Artifacts {
    directory: PathBuf,
    run_id: String,
    samples: BufWriter<File>,
}

impl Artifacts {
    fn new(directory: &Path, run_id: &str) -> Self {
        assert!(
            run_id.len() == 40 && run_id.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "performance run ID must be a 40-digit hexadecimal identity"
        );
        fs::create_dir_all(directory).expect("create performance artifact directory");
        let mut samples = BufWriter::new(
            File::create(directory.join("samples.tsv")).expect("create performance samples"),
        );
        writeln!(
            samples,
            "run_id\tengine\tworkload\titeration\torder\tposition\telapsed_ns\texit_code\tvalidated"
        )
        .expect("write sample header");
        samples.flush().expect("flush sample header");
        fs::write(
            directory.join("summary.tsv"),
            b"status\tmessage\nstarted\tbenchmark in progress\n",
        )
        .expect("initialize performance summary");
        Self {
            directory: directory.to_path_buf(),
            run_id: run_id.to_string(),
            samples,
        }
    }

    fn write_metadata(
        &self,
        engines: &Engines,
        tools: &PerfTools,
        provider: (&Path, &ProviderIdentity),
        evidence: &EvidenceIdentity,
        context: MetadataContext<'_>,
    ) {
        let (provider_root, provider_identity) = provider;
        assert_eq!(self.run_id, evidence.run_id);
        let cpu_count = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1);
        assert_eq!(
            context.runner_image,
            validate_public_runner_label(context.runner_image).unwrap_or_else(|| panic!(
                "runner image must be pre-normalized by the benchmark driver"
            ))
        );
        let mut rows = vec![
            ("format", "performance-metadata-v2".to_string()),
            ("run_id", self.run_id.clone()),
            ("shell_commit", shell_baseline_sha().to_string()),
            ("shell_tree", evidence.shell_tree.clone()),
            ("rust_commit", engines.rust.commit.clone()),
            ("rust_tree", evidence.rust_tree.clone()),
            ("rust_worktree_dirty", context.dirty.to_string()),
            ("profile", "release".to_string()),
            ("samples", RUNS.to_string()),
            ("startup_warmups", STARTUP_WARMUPS.to_string()),
            ("update_warmups", UPDATE_WARMUPS.to_string()),
            ("disjoint_overlays", OVERLAYS.to_string()),
            ("feature_selected_overlays", OVERLAYS.to_string()),
            ("feature_declared_overlays", (OVERLAYS + 1).to_string()),
            ("files_per_overlay", FILES_PER_OVERLAY.to_string()),
            ("os", std::env::consts::OS.to_string()),
            ("arch", std::env::consts::ARCH.to_string()),
            ("cpu_count", cpu_count.to_string()),
            ("cpu_class", public_cpu_class(cpu_count).to_string()),
            ("runner_image", context.runner_image.to_string()),
            ("command_timeout_ns", COMMAND_TIMEOUT.as_nanos().to_string()),
            ("artifact_location", "configured-output".to_string()),
            ("client_path_policy", "controlled".to_string()),
        ];
        for (name, path, args) in [
            ("git", &tools.git, ["--version"].as_slice()),
            ("bash", &tools.bash, ["--version"].as_slice()),
            ("cargo", &tools.cargo, ["--version"].as_slice()),
            ("rustc", &tools.rustc, ["--version"].as_slice()),
        ] {
            rows.push((
                match name {
                    "git" => "git_location",
                    "bash" => "bash_location",
                    "cargo" => "cargo_location",
                    _ => "rustc_location",
                },
                match name {
                    "git" | "bash" => "system-tool",
                    _ => "selected-build-tool",
                }
                .to_string(),
            ));
            rows.push((
                match name {
                    "git" => "git_version",
                    "bash" => "bash_version",
                    "cargo" => "cargo_version",
                    _ => "rustc_version",
                },
                tool_version(name, path, args),
            ));
            rows.push((
                match name {
                    "git" => "git_blob",
                    "bash" => "bash_blob",
                    "cargo" => "cargo_blob",
                    _ => "rustc_blob",
                },
                match name {
                    "git" => evidence.git_blob.clone(),
                    "bash" => evidence.bash_blob.clone(),
                    "cargo" => evidence.cargo_blob.clone(),
                    _ => evidence.rustc_blob.clone(),
                },
            ));
        }
        for name in ["shell", "rust"] {
            rows.push((
                if name == "shell" {
                    "shell_executable_location"
                } else {
                    "rust_executable_location"
                },
                if name == "shell" {
                    "historical-checkout/bin/dot"
                } else {
                    "run-private-target/dot/release/dot"
                }
                .to_string(),
            ));
            rows.push((
                if name == "shell" {
                    "shell_executable_blob"
                } else {
                    "rust_executable_blob"
                },
                if name == "shell" {
                    evidence.shell_executable_blob.clone()
                } else {
                    evidence.rust_executable_blob.clone()
                },
            ));
        }
        rows.extend([
            ("shdeps_location", "provider-checkout".to_string()),
            (
                "current_shdeps_lock_revision",
                provider_identity.current_revision.clone(),
            ),
            (
                "shell_shdeps_lock_revision",
                provider_identity.shell_revision.clone(),
            ),
            ("shdeps_abi", provider_identity.abi.clone()),
            (
                "shdeps_commit",
                git_text(tools, provider_root, &["rev-parse", "HEAD"]),
            ),
            ("shdeps_tree", evidence.shdeps_tree.clone()),
            (
                "shdeps_executable_location",
                "run-private-target/shdeps/release/shdeps".to_string(),
            ),
            (
                "shdeps_executable_blob",
                evidence.shdeps_executable_blob.clone(),
            ),
        ]);
        rows.extend(
            filesystem_probe_targets(
                &engines.rust.source_root,
                &engines.rust.executable,
                context.fixture_root,
            )
            .into_iter()
            .map(|(key, path)| (key, filesystem_description(&path))),
        );
        let mut file = BufWriter::new(
            File::create(self.directory.join("metadata.tsv")).expect("create performance metadata"),
        );
        writeln!(file, "key\tvalue").expect("write metadata header");
        for (key, value) in rows {
            writeln!(file, "{}\t{}", tsv(key), tsv(&value)).expect("write metadata row");
        }
        file.flush().expect("flush performance metadata");
    }

    fn record_first_spawn(
        &mut self,
        elapsed_ns: u128,
        validation: Result<(), String>,
    ) -> Result<(), String> {
        validation?;
        writeln!(
            self.samples,
            "{}\trust\tfirst-spawn\t0\trust-only\t1\t{elapsed_ns}\t0\ttrue",
            self.run_id
        )
        .expect("write first-spawn sample");
        self.samples.flush().expect("flush first-spawn sample");
        Ok(())
    }

    fn record_pair(
        &mut self,
        workload: Workload,
        iteration: usize,
        order: [EngineKind; 2],
        shell_ns: u128,
        rust_ns: u128,
        exit_code: i32,
    ) {
        let order_label = format!("{}-{}", order[0].label(), order[1].label());
        for (position, kind) in order.into_iter().enumerate() {
            let elapsed_ns = match kind {
                EngineKind::Shell => shell_ns,
                EngineKind::Rust => rust_ns,
            };
            writeln!(
                self.samples,
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\ttrue",
                self.run_id,
                kind.label(),
                workload.label(),
                iteration + 1,
                order_label,
                position + 1,
                elapsed_ns,
                exit_code,
            )
            .expect("write performance sample");
        }
        self.samples.flush().expect("flush validated pair");
    }

    fn write_results(
        &mut self,
        rows: &[SummaryRow],
        evidence: &EvidenceIdentity,
        tools: &PerfTools,
    ) {
        self.samples.flush().expect("flush all performance samples");
        let mut file = BufWriter::new(
            File::create(self.directory.join("summary.tsv")).expect("replace performance summary"),
        );
        writeln!(
            file,
            "run_id\tworkload\tsamples\tshell_median_ns\tshell_p95_ns\trust_median_ns\trust_p95_ns\tmax_rust_percent\trust_p95_budget_ns\tresult"
        )
        .expect("write summary header");
        for row in rows {
            let (shell_median, shell_p95) = row
                .shell
                .map(|stats| (stats.median_ns.to_string(), stats.p95_ns.to_string()))
                .unwrap_or_else(|| (String::new(), String::new()));
            let max_percent = row
                .max_rust_percent
                .map(|value| value.to_string())
                .unwrap_or_default();
            writeln!(
                file,
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                self.run_id,
                row.workload.label(),
                row.samples,
                shell_median,
                shell_p95,
                row.rust.median_ns,
                row.rust.p95_ns,
                max_percent,
                row.rust_p95_budget_ns,
                if row.passed { "pass" } else { "fail" },
            )
            .expect("write performance summary row");
        }
        file.flush().expect("flush performance summary");
        drop(file);
        self.write_calibration(rows, evidence, tools);
    }

    fn write_calibration(
        &self,
        rows: &[SummaryRow],
        evidence: &EvidenceIdentity,
        tools: &PerfTools,
    ) {
        let metadata_blob = file_blob(tools, &self.directory.join("metadata.tsv"));
        let samples_blob = file_blob(tools, &self.directory.join("samples.tsv"));
        let summary_blob = file_blob(tools, &self.directory.join("summary.tsv"));
        let mut file = BufWriter::new(
            File::create(self.directory.join("calibration.tsv"))
                .expect("create bound performance calibration"),
        );
        writeln!(
            file,
            "run_id\tshell_commit\tshell_tree\trust_commit\trust_tree\tshdeps_commit\tshdeps_tree\tgit_blob\tbash_blob\tcargo_blob\trustc_blob\tshell_executable_blob\trust_executable_blob\tshdeps_executable_blob\tmetadata_blob\tsamples_blob\tsummary_blob\tworkload\tsamples\tshell_median_ns\trust_median_ns\trust_p95_ns\trust_p95_budget_ns\tobserved_rust_percent\tbudget_headroom_percent"
        )
        .expect("write calibration header");
        for row in rows {
            let shell_median = row
                .shell
                .map(|stats| stats.median_ns.to_string())
                .unwrap_or_default();
            let observed_percent = row
                .shell
                .and_then(|stats| ratio_percent_ceil(row.rust.median_ns, stats.median_ns))
                .map(|percent| percent.to_string())
                .unwrap_or_default();
            writeln!(
                file,
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                evidence.run_id,
                evidence.shell_commit,
                evidence.shell_tree,
                evidence.rust_commit,
                evidence.rust_tree,
                evidence.shdeps_commit,
                evidence.shdeps_tree,
                evidence.git_blob,
                evidence.bash_blob,
                evidence.cargo_blob,
                evidence.rustc_blob,
                evidence.shell_executable_blob,
                evidence.rust_executable_blob,
                evidence.shdeps_executable_blob,
                metadata_blob,
                samples_blob,
                summary_blob,
                row.workload.label(),
                row.samples,
                shell_median,
                row.rust.median_ns,
                row.rust.p95_ns,
                row.rust_p95_budget_ns,
                observed_percent,
                budget_headroom_percent(row.rust.p95_ns, row.rust_p95_budget_ns),
            )
            .expect("write calibration row");
        }
        file.flush().expect("flush bound performance calibration");
    }
}

fn tsv(value: &str) -> String {
    value.replace(['\t', '\n', '\r'], " ")
}

fn parse_public_tool_version(name: &str, output: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(output).ok()?;
    let line = text.lines().next()?;
    let prefix = match name {
        "git" => "git version ",
        "bash" => "GNU bash, version ",
        "cargo" => "cargo ",
        "rustc" => "rustc ",
        _ => return None,
    };
    let version = line.strip_prefix(prefix)?.split_ascii_whitespace().next()?;
    if !public_version_token(name, version) {
        return None;
    }
    Some(format!("{name} {version}"))
}

fn numeric_version_core(value: &str, minimum: usize, maximum: usize) -> bool {
    let components = value.split('.').collect::<Vec<_>>();
    (minimum..=maximum).contains(&components.len())
        && components.iter().all(|component| {
            !component.is_empty()
                && component.len() <= 6
                && component.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn public_version_token(name: &str, version: &str) -> bool {
    if version.is_empty() || version.len() > 64 {
        return false;
    }
    match name {
        "git" => numeric_version_core(version, 2, 4),
        "cargo" | "rustc" => {
            let (core, qualifier) = version
                .split_once('-')
                .map_or((version, None), |(core, qualifier)| (core, Some(qualifier)));
            numeric_version_core(core, 3, 3)
                && qualifier.is_none_or(|qualifier| {
                    let mut fields = qualifier.split('.');
                    let Some(kind) = fields.next() else {
                        return false;
                    };
                    matches!(kind, "alpha" | "beta" | "dev" | "nightly")
                        && fields.next().is_none_or(|number| {
                            !number.is_empty()
                                && number.len() <= 6
                                && number.bytes().all(|byte| byte.is_ascii_digit())
                        })
                        && fields.next().is_none()
                })
        }
        "bash" => {
            let Some(body) = version.strip_suffix("-release") else {
                return false;
            };
            let Some((core, patch)) = body.split_once('(') else {
                return false;
            };
            let Some(patch) = patch.strip_suffix(')') else {
                return false;
            };
            numeric_version_core(core, 3, 3)
                && !patch.is_empty()
                && patch.len() <= 6
                && patch.bytes().all(|byte| byte.is_ascii_digit())
        }
        _ => false,
    }
}

fn tool_version(name: &str, program: &Path, args: &[&str]) -> String {
    let mut command = Command::new(program);
    command.args(args).env_clear().env("LC_ALL", "C");
    // Rustup shims resolve the active toolchain from these variables; without
    // them the probe fails even though `cargo test` itself runs through the
    // same shim. Only the parsed version line is recorded, so passing the
    // resolution environment cannot leak private paths into the metadata.
    if matches!(name, "cargo" | "rustc") {
        for key in ["RUSTUP_TOOLCHAIN", "RUSTUP_HOME", "CARGO_HOME", "HOME"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
    }
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("run {name} metadata command: {error}"));
    assert!(output.status.success(), "{name} metadata command failed");
    parse_public_tool_version(name, &output.stdout)
        .unwrap_or_else(|| panic!("{name} emitted an invalid public version"))
}

fn decode_mountinfo_path(field: &[u8]) -> Option<PathBuf> {
    let mut decoded = Vec::with_capacity(field.len());
    let mut index = 0;
    while index < field.len() {
        if field[index] == b'\\' {
            let digits = field.get(index + 1..index + 4)?;
            if !digits.iter().all(|digit| matches!(digit, b'0'..=b'7')) {
                return None;
            }
            decoded.push((digits[0] - b'0') * 64 + (digits[1] - b'0') * 8 + (digits[2] - b'0'));
            index += 4;
        } else {
            decoded.push(field[index]);
            index += 1;
        }
    }
    Some(PathBuf::from(OsString::from_vec(decoded)))
}

fn safe_mount_options(fields: &[&[u8]]) -> String {
    const ALLOWED: [&[u8]; 11] = [
        b"async",
        b"dirsync",
        b"lazytime",
        b"noatime",
        b"nodev",
        b"nodiratime",
        b"noexec",
        b"nosuid",
        b"relatime",
        b"ro",
        b"rw",
    ];
    let mut options = fields
        .iter()
        .flat_map(|field| field.split(|byte| *byte == b','))
        .filter(|option| ALLOWED.contains(option))
        .map(|option| String::from_utf8_lossy(option).into_owned())
        .collect::<Vec<_>>();
    options.sort();
    options.dedup();
    options.join(",")
}

fn public_filesystem_type(filesystem: &[u8]) -> &'static str {
    match filesystem {
        b"btrfs" => "btrfs",
        b"ext2" => "ext2",
        b"ext3" => "ext3",
        b"ext4" => "ext4",
        b"f2fs" => "f2fs",
        b"overlay" => "overlay",
        b"tmpfs" => "tmpfs",
        b"xfs" => "xfs",
        b"zfs" => "zfs",
        _ => "other",
    }
}

fn public_cpu_class(count: usize) -> &'static str {
    match count {
        0 | 1 => "single",
        2..=4 => "small",
        5..=16 => "medium",
        _ => "large",
    }
}

fn public_runner_image(value: Option<&OsStr>) -> &'static str {
    match value.map(|value| value.as_bytes()) {
        Some(b"ubuntu20") => "ubuntu20",
        Some(b"ubuntu22") => "ubuntu22",
        Some(b"ubuntu24") => "ubuntu24",
        None | Some(b"") => "local",
        _ => "other",
    }
}

fn validate_public_runner_label(value: &str) -> Option<&str> {
    matches!(
        value,
        "local" | "other" | "ubuntu20" | "ubuntu22" | "ubuntu24"
    )
    .then_some(value)
}

fn filesystem_description_from_mountinfo(path: &Path, mountinfo: &[u8]) -> Option<String> {
    let mut selected: Option<(PathBuf, &[u8], String)> = None;
    for line in mountinfo.split(|byte| *byte == b'\n') {
        let fields = line
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        let Some(separator) = fields.iter().position(|field| *field == b"-") else {
            continue;
        };
        if fields.len() <= separator + 3 || fields.len() <= 5 {
            continue;
        }
        let mount = decode_mountinfo_path(fields[4])?;
        if !path.starts_with(&mount)
            || selected
                .as_ref()
                .is_some_and(|(best, _, _)| best.as_os_str().len() >= mount.as_os_str().len())
        {
            continue;
        }
        selected = Some((
            mount,
            fields[separator + 1],
            safe_mount_options(&[fields[5], fields[separator + 3]]),
        ));
    }
    selected.map(|(_, filesystem, options)| {
        format!(
            "type={};options={options}",
            public_filesystem_type(filesystem)
        )
    })
}

fn filesystem_description(path: &Path) -> String {
    let mountinfo = fs::read("/proc/self/mountinfo").expect("read Linux mount metadata");
    filesystem_description_from_mountinfo(path, &mountinfo)
        .unwrap_or_else(|| panic!("no filesystem metadata for benchmark location"))
}

fn file_blob(tools: &PerfTools, path: &Path) -> String {
    let output = Command::new(&tools.git)
        .args([OsStr::new("hash-object"), OsStr::new("--no-filters")])
        .arg(path)
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &tools.client_path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap_or_else(|error| panic!("fingerprint {}: {error}", path.display()));
    assert!(
        output.status.success(),
        "fingerprint {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("Git object id is UTF-8")
        .trim()
        .to_string()
}

fn filesystem_probe_targets(
    source_root: &Path,
    binary: &Path,
    fixture_root: &Path,
) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("fixture_filesystem", fixture_root.to_path_buf()),
        ("source_filesystem", source_root.to_path_buf()),
        (
            "binary_filesystem",
            binary.parent().unwrap_or(binary).to_path_buf(),
        ),
    ]
}

fn git_output(tools: &PerfTools, dir: &Path, args: &[&str]) -> Output {
    let output = Command::new(&tools.git)
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", &tools.client_path)
        .env("HOME", dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_COUNT", "3")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", "/dev/null")
        .env("GIT_CONFIG_KEY_1", "commit.gpgSign")
        .env("GIT_CONFIG_VALUE_1", "false")
        .env("GIT_CONFIG_KEY_2", "tag.gpgSign")
        .env("GIT_CONFIG_VALUE_2", "false")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git(tools: &PerfTools, dir: &Path, args: &[&str]) {
    let _ = git_output(tools, dir, args);
}

fn git_text(tools: &PerfTools, dir: &Path, args: &[&str]) -> String {
    String::from_utf8(git_output(tools, dir, args).stdout)
        .expect("git output is UTF-8")
        .trim()
        .to_string()
}

fn engines(tools: &PerfTools) -> Engines {
    if std::hint::black_box(cfg!(debug_assertions)) {
        panic!("performance timing requires `cargo test --release`");
    }
    let current_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let shell_root =
        PathBuf::from(std::env::var_os(SHELL_ROOT_ENV).unwrap_or_else(|| {
            panic!("{SHELL_ROOT_ENV} is required; use scripts/benchmark-port.sh")
        }));
    let current_sha = std::env::var(CURRENT_SHA_ENV)
        .unwrap_or_else(|_| panic!("{CURRENT_SHA_ENV} is required; use scripts/benchmark-port.sh"));
    assert_eq!(
        git_text(tools, &current_root, &["rev-parse", "HEAD"]),
        current_sha,
        "native source identity does not match the benchmark driver"
    );
    assert_eq!(
        git_text(tools, &shell_root, &["rev-parse", "HEAD"]),
        shell_baseline_sha(),
        "shell checkout is not the pinned pre-cutover revision"
    );
    assert!(
        shell_root.join("lib/dot/main.sh").is_file(),
        "shell baseline lacks the private Bash engine"
    );
    assert!(
        !current_root.join("lib/dot/main.sh").exists(),
        "native source unexpectedly retains the private Bash engine"
    );
    let shell_executable = shell_root.join("bin/dot");
    let rust_executable = PathBuf::from(env!("CARGO_BIN_EXE_dot"));
    assert_eq!(
        rust_executable.parent().and_then(Path::file_name),
        Some(OsStr::new("release")),
        "native performance binary is not from Cargo's release profile"
    );
    assert_ne!(
        fs::canonicalize(&shell_executable).expect("canonical shell executable"),
        fs::canonicalize(&rust_executable).expect("canonical rust executable"),
        "shell and Rust benchmark executables must be distinct"
    );
    Engines {
        shell: Engine {
            kind: EngineKind::Shell,
            executable: shell_executable,
            source_root: shell_root,
            commit: shell_baseline_sha().to_string(),
        },
        rust: Engine {
            kind: EngineKind::Rust,
            executable: rust_executable,
            source_root: current_root,
            commit: current_sha,
        },
    }
}

fn lock_value(root: &Path, key: &str) -> Option<String> {
    fs::read_to_string(root.join("support/shdeps.lock"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")).map(str::to_string))
}

fn validate_provider(
    tools: &PerfTools,
    engines: &Engines,
    root: &Path,
    provider_binary: &Path,
) -> Result<ProviderIdentity, String> {
    let current_revision = lock_value(&engines.rust.source_root, "revision")
        .ok_or_else(|| "native Shdeps lock has no revision".to_string())?;
    let shell_revision = lock_value(&engines.shell.source_root, "revision")
        .ok_or_else(|| "shell Shdeps lock has no revision".to_string())?;
    if ![&current_revision, &shell_revision]
        .into_iter()
        .all(|revision| {
            revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        return Err("Shdeps lock revision is not a 40-digit commit".to_string());
    }
    let current_abi = lock_value(&engines.rust.source_root, "abi")
        .ok_or_else(|| "native Shdeps lock has no ABI".to_string())?;
    let shell_abi = lock_value(&engines.shell.source_root, "abi")
        .ok_or_else(|| "shell Shdeps lock has no ABI".to_string())?;
    if current_abi != shell_abi {
        return Err(format!(
            "current Shdeps ABI {current_abi} is incompatible with shell ABI {shell_abi}"
        ));
    }
    let observed = git_text(tools, root, &["rev-parse", "HEAD"]);
    if observed != current_revision {
        return Err(format!(
            "Shdeps checkout is {observed}, expected current lock {current_revision}"
        ));
    }
    for relative in ["install.sh", "shdeps.sh"] {
        if !root.join(relative).is_file() {
            return Err(format!("Shdeps provider is missing {relative}"));
        }
    }
    canonical_executable(provider_binary)
        .map_err(|error| format!("invalid run-private Shdeps executable: {error}"))?;
    Ok(ProviderIdentity {
        current_revision,
        shell_revision,
        abi: current_abi,
    })
}

fn provider_root(tools: &PerfTools, engines: &Engines) -> (PathBuf, PathBuf, ProviderIdentity) {
    let root =
        PathBuf::from(std::env::var_os(SHDEPS_ROOT_ENV).unwrap_or_else(|| {
            panic!("{SHDEPS_ROOT_ENV} is required; use scripts/benchmark-port.sh")
        }));
    let provider_binary = required_tool(SHDEPS_BINARY_ENV)
        .unwrap_or_else(|error| panic!("invalid Shdeps provider binary: {error}"));
    let identity = validate_provider(tools, engines, &root, &provider_binary)
        .unwrap_or_else(|error| panic!("invalid Shdeps provider: {error}"));
    (root, provider_binary, identity)
}

fn client_env(command: &mut Command, engine: &Engine, client: &Client, tools: &PerfTools) {
    command.env_clear();
    command.env("LC_ALL", "C");
    command.env("PATH", &tools.client_path);
    command.env("TMPDIR", &client.tmp);
    command.env("HOME", &client.home);
    command.env("XDG_STATE_HOME", &client.state);
    command.env("XDG_CONFIG_HOME", &client.xdg);
    command.env("XDG_DATA_HOME", &client.data);
    command.env("XDG_CACHE_HOME", &client.cache);
    command.env("DOT_SOURCE_ROOT", &engine.source_root);
    command.env("GIT_AUTHOR_NAME", "fixture");
    command.env("GIT_AUTHOR_EMAIL", "fixture@example.invalid");
    command.env("GIT_COMMITTER_NAME", "fixture");
    command.env("GIT_COMMITTER_EMAIL", "fixture@example.invalid");
    command.env("GIT_CONFIG_NOSYSTEM", "1");
    command.env("GIT_CONFIG_GLOBAL", "/dev/null");
    command.env("GIT_TERMINAL_PROMPT", "0");
    command.env("GIT_CONFIG_COUNT", "3");
    command.env("GIT_CONFIG_KEY_0", "core.hooksPath");
    command.env("GIT_CONFIG_VALUE_0", "/dev/null");
    command.env("GIT_CONFIG_KEY_1", "commit.gpgSign");
    command.env("GIT_CONFIG_VALUE_1", "false");
    command.env("GIT_CONFIG_KEY_2", "tag.gpgSign");
    command.env("GIT_CONFIG_VALUE_2", "false");
    // The shell implementation can discover its Bash parent even when SHELL
    // is absent; the native engine intentionally consumes the exported login
    // shell. Pin the normal login-session input so the benchmark compares the
    // supported contract instead of an `env -i` artifact.
    command.env("DOT_BASH", &tools.bash);
    command.env("SHELL", &tools.bash);
    for (key, value) in &client.extra_env {
        command.env(key, value);
    }
    command.current_dir(&client.home);
}

#[cfg(target_os = "linux")]
fn wait_without_reaping(pid: u32) -> std::io::Result<()> {
    loop {
        // SAFETY: waitid writes only the local siginfo value. WNOWAIT retains
        // the owned leader as a zombie so its PID/session identity cannot be
        // reused before descendant cleanup completes.
        unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            if libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) == 0 {
                return Ok(());
            }
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct SessionMember {
    key: ProcessKey,
    pidfd: OwnedFd,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ProcessKey {
    pid: u32,
    start: u64,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Eq, PartialEq)]
struct LinuxProcessIdentity {
    parent: u32,
    session: u32,
    start: u64,
    live: bool,
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct ProcessBoundary {
    leader: ProcessKey,
    supervisor: u32,
    baseline_direct: HashSet<ProcessKey>,
    observed: Mutex<HashSet<ProcessKey>>,
    retained: Mutex<HashMap<ProcessKey, OwnedFd>>,
    scan_fault: Mutex<ScanFault>,
}

#[cfg(target_os = "linux")]
enum DirectChildInventory {
    Available(HashSet<u32>),
    Unavailable,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
struct ScanFault {
    omit_retained_then_fail: bool,
    omit_unretained_then_fail: bool,
    hide_direct_once: bool,
    fail_broad_after_direct: bool,
    direct_pid_file: Option<PathBuf>,
    phase: u8,
    omitted_pid: Option<u32>,
}

#[cfg(target_os = "linux")]
fn linux_process_identity(pid: u32) -> Result<Option<LinuxProcessIdentity>, String> {
    let stat = match fs::read(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if process_vanished(&error) => return Ok(None),
        Err(error) => return Err(format!("read /proc/{pid}/stat: {error}")),
    };
    parse_linux_process_identity(pid, &stat).map(Some)
}

#[cfg(target_os = "linux")]
fn process_vanished(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOENT) | Some(libc::ESRCH))
}

#[cfg(target_os = "linux")]
fn parse_linux_process_identity(pid: u32, stat: &[u8]) -> Result<LinuxProcessIdentity, String> {
    let delimiter = stat
        .windows(2)
        .rposition(|window| window == b") ")
        .ok_or_else(|| format!("malformed /proc/{pid}/stat"))?;
    let fields = stat[delimiter + 2..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    if fields.len() <= 19 {
        return Err(format!("short /proc/{pid}/stat"));
    }
    let parent = std::str::from_utf8(fields[1])
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| format!("invalid parent in /proc/{pid}/stat"))?;
    let session = std::str::from_utf8(fields[3])
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| format!("invalid session in /proc/{pid}/stat"))?;
    let start = std::str::from_utf8(fields[19])
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| format!("invalid start time in /proc/{pid}/stat"))?;
    Ok(LinuxProcessIdentity {
        parent,
        session,
        start,
        live: !matches!(fields[0], b"Z" | b"X" | b"x"),
    })
}

#[cfg(target_os = "linux")]
fn open_process_member(
    pid: u32,
    expected: &LinuxProcessIdentity,
) -> Result<Option<SessionMember>, String> {
    // SAFETY: pidfd_open takes only the numeric PID and flags. The returned
    // descriptor binds later signals to this process instance, not a reused
    // numeric PID.
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if descriptor < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        return Err(format!("open pidfd for {pid}: {error}"));
    }
    // SAFETY: pidfd_open returned a new owned descriptor.
    let pidfd = unsafe { OwnedFd::from_raw_fd(descriptor as i32) };
    if linux_process_identity(pid)?.as_ref() != Some(expected) {
        return Ok(None);
    }
    Ok(Some(SessionMember {
        key: ProcessKey {
            pid,
            start: expected.start,
        },
        pidfd,
    }))
}

#[cfg(target_os = "linux")]
struct ExactProcessGuard {
    member: Option<SessionMember>,
}

#[cfg(target_os = "linux")]
impl ExactProcessGuard {
    fn acquire(pid: u32) -> Result<Self, String> {
        let member = match linux_process_identity(pid)? {
            Some(identity) => open_process_member(pid, &identity)?,
            None => None,
        };
        Ok(Self { member })
    }

    fn is_live(&self) -> bool {
        self.member
            .as_ref()
            .is_some_and(|member| !pidfd_is_ready(&member.pidfd).unwrap_or(true))
    }

    fn terminate(&mut self) -> Result<(), String> {
        let Some(member) = self.member.take() else {
            return Ok(());
        };
        // SAFETY: the pidfd binds the signal to the exact fixture identity.
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                member.pidfd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            self.member = Some(member);
            return Err(format!(
                "kill exact fixture identity: {}",
                std::io::Error::last_os_error()
            ));
        }
        let deadline = Instant::now() + FORCED_CLEANUP_GRACE;
        while !pidfd_is_ready(&member.pidfd)? {
            if Instant::now() >= deadline {
                self.member = Some(member);
                return Err("timed out terminating exact fixture identity".to_string());
            }
            std::thread::sleep(PROCESS_POLL);
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl Drop for ExactProcessGuard {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

#[cfg(target_os = "linux")]
fn linux_process_table() -> Result<HashMap<u32, LinuxProcessIdentity>, String> {
    let mut processes = HashMap::new();
    let entries = fs::read_dir("/proc").map_err(|error| format!("read /proc: {error}"))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read /proc entry: {error}"))?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if let Some(identity) = linux_process_identity(pid)? {
            processes.insert(pid, identity);
        }
    }
    Ok(processes)
}

#[cfg(target_os = "linux")]
fn ensure_child_subreaper() -> Result<(), String> {
    static SUBREAPER: OnceLock<Result<(), i32>> = OnceLock::new();
    let result = SUBREAPER.get_or_init(|| {
        // SAFETY: prctl receives the documented integer-only
        // PR_SET_CHILD_SUBREAPER operation. The setting is process-wide and
        // intentionally remains enabled for this dedicated test executable.
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
        }
    });
    result.map_err(|errno| format!("enable child subreaper: errno {errno}"))
}

#[cfg(target_os = "linux")]
fn direct_child_pids(parent: u32) -> Result<DirectChildInventory, String> {
    let task_root = format!("/proc/{parent}/task");
    let mut children = HashSet::new();
    let mut readable_records = 0_usize;
    for entry in
        fs::read_dir(&task_root).map_err(|error| format!("read process task table: {error}"))?
    {
        let entry = entry.map_err(|error| format!("read process task entry: {error}"))?;
        let Some(task) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let record = match fs::read_to_string(format!("{task_root}/{task}/children")) {
            Ok(record) => {
                readable_records += 1;
                record
            }
            Err(error) if process_vanished(&error) => continue,
            Err(error) => return Err(format!("read direct child identities: {error}")),
        };
        for field in record.split_ascii_whitespace() {
            let child = field
                .parse::<u32>()
                .map_err(|_| "invalid direct child identity".to_string())?;
            if child > 0 {
                children.insert(child);
            }
        }
    }
    if readable_records == 0 {
        Ok(DirectChildInventory::Unavailable)
    } else {
        Ok(DirectChildInventory::Available(children))
    }
}

#[cfg(target_os = "linux")]
fn direct_child_keys(parent: u32) -> Result<Option<HashSet<ProcessKey>>, String> {
    let mut children = HashSet::new();
    let DirectChildInventory::Available(pids) = direct_child_pids(parent)? else {
        return Ok(None);
    };
    for pid in pids {
        let Some(identity) = linux_process_identity(pid)? else {
            continue;
        };
        if identity.parent == parent {
            children.insert(ProcessKey {
                pid,
                start: identity.start,
            });
        }
    }
    Ok(Some(children))
}

#[cfg(target_os = "linux")]
fn process_boundary(
    leader: u32,
    baseline_direct: HashSet<ProcessKey>,
) -> Result<ProcessBoundary, String> {
    let identity = linux_process_identity(leader)?
        .ok_or_else(|| "command leader vanished before identity validation".to_string())?;
    if identity.session != leader {
        return Err("command leader did not establish the expected session".to_string());
    }
    let leader_member = open_process_member(leader, &identity)?
        .ok_or_else(|| "command leader vanished before pidfd validation".to_string())?;
    let leader = leader_member.key;
    let scan_fault = match std::env::var_os(SUPERVISOR_SCAN_FAULT_ENV) {
        None => ScanFault::default(),
        Some(value) if value == "omit-then-fail" => ScanFault {
            omit_retained_then_fail: true,
            omit_unretained_then_fail: false,
            hide_direct_once: false,
            fail_broad_after_direct: false,
            direct_pid_file: None,
            phase: 0,
            omitted_pid: None,
        },
        Some(value) if value == "omit-new-then-fail" => ScanFault {
            omit_retained_then_fail: false,
            omit_unretained_then_fail: true,
            hide_direct_once: false,
            fail_broad_after_direct: false,
            direct_pid_file: None,
            phase: 0,
            omitted_pid: None,
        },
        Some(value) if value == "unobserved-then-fail" => ScanFault {
            omit_retained_then_fail: false,
            omit_unretained_then_fail: true,
            hide_direct_once: true,
            fail_broad_after_direct: false,
            direct_pid_file: None,
            phase: 0,
            omitted_pid: None,
        },
        Some(value) if value == "direct-before-fail" => ScanFault {
            omit_retained_then_fail: false,
            omit_unretained_then_fail: false,
            hide_direct_once: false,
            fail_broad_after_direct: true,
            direct_pid_file: std::env::var_os(SUPERVISOR_DIRECT_PID_FILE_ENV).map(PathBuf::from),
            phase: 0,
            omitted_pid: None,
        },
        Some(_) => return Err("invalid supervisor process-scan fault mode".to_string()),
    };
    Ok(ProcessBoundary {
        leader,
        supervisor: std::process::id(),
        baseline_direct,
        observed: Mutex::new(HashSet::from([leader])),
        retained: Mutex::new(HashMap::from([(leader, leader_member.pidfd)])),
        scan_fault: Mutex::new(scan_fault),
    })
}

#[cfg(target_os = "linux")]
fn pidfd_is_ready(pidfd: &OwnedFd) -> Result<bool, String> {
    let mut descriptor = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: descriptor points to one initialized pollfd record.
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if result >= 0 {
            if descriptor.revents & libc::POLLNVAL != 0 {
                return Err("retained process identity handle became invalid".to_string());
            }
            return Ok(result > 0
                && descriptor.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(format!("poll retained process identity: {error}"));
        }
    }
}

#[cfg(target_os = "linux")]
fn prune_retained_members(boundary: &ProcessBoundary) -> Result<Vec<ProcessKey>, String> {
    let mut retained = boundary
        .retained
        .lock()
        .map_err(|_| "retained process-boundary map was poisoned".to_string())?;
    let mut exited = Vec::new();
    for (&key, pidfd) in retained.iter() {
        if pidfd_is_ready(pidfd)? {
            exited.push(key);
        }
    }
    for key in exited {
        retained.remove(&key);
    }
    Ok(retained.keys().copied().collect())
}

#[cfg(target_os = "linux")]
fn retain_adopted_children(boundary: &ProcessBoundary) -> Result<bool, String> {
    let forced_pid = {
        let fault = boundary
            .scan_fault
            .lock()
            .map_err(|_| "process-scan fault state was poisoned".to_string())?;
        fault
            .direct_pid_file
            .as_ref()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|value| value.trim().parse::<u32>().ok())
    };
    let mut inventory = direct_child_pids(boundary.supervisor)?;
    if let Some(pid) = forced_pid {
        match &mut inventory {
            DirectChildInventory::Available(pids) => {
                pids.insert(pid);
            }
            DirectChildInventory::Unavailable => {
                inventory = DirectChildInventory::Available(HashSet::from([pid]));
            }
        }
    }
    let DirectChildInventory::Available(pids) = inventory else {
        return Ok(false);
    };
    let mut retained = boundary
        .retained
        .lock()
        .map_err(|_| "retained process-boundary map was poisoned".to_string())?;
    for pid in pids {
        if pid == boundary.leader.pid {
            continue;
        }
        let Some(identity) = linux_process_identity(pid)? else {
            continue;
        };
        let key = ProcessKey {
            pid,
            start: identity.start,
        };
        if identity.parent != boundary.supervisor
            || !identity.live
            || identity.start < boundary.leader.start
            || boundary.baseline_direct.contains(&key)
            || retained.contains_key(&key)
        {
            continue;
        }
        if let Some(member) = open_process_member(pid, &identity)? {
            retained.insert(member.key, member.pidfd);
        }
    }
    Ok(true)
}

#[cfg(target_os = "linux")]
fn live_boundary_members(boundary: &ProcessBoundary) -> Result<Vec<ProcessKey>, String> {
    prune_retained_members(boundary)?;
    let hide_direct = {
        let fault = boundary
            .scan_fault
            .lock()
            .map_err(|_| "process-scan fault state was poisoned".to_string())?;
        fault.hide_direct_once
    };
    let direct_inventory = if hide_direct {
        Ok(false)
    } else {
        retain_adopted_children(boundary)
    };
    let injected_scan_error = {
        let mut fault = boundary
            .scan_fault
            .lock()
            .map_err(|_| "process-scan fault state was poisoned".to_string())?;
        let direct_fixture_ready = fault.fail_broad_after_direct
            && fault
                .direct_pid_file
                .as_ref()
                .is_some_and(|path| fs::read_to_string(path).is_ok());
        if direct_fixture_ready {
            Some("injected process-table failure after direct-child discovery".to_string())
        } else if (fault.omit_retained_then_fail || fault.omit_unretained_then_fail)
            && fault.phase == 1
        {
            fault.phase = 2;
            Some(match fault.omitted_pid {
                Some(pid) if fault.omit_unretained_then_fail => {
                    format!("injected process-table failure after unretained omission of {pid}")
                }
                _ => "injected process-table failure after omission".to_string(),
            })
        } else if (fault.omit_retained_then_fail || fault.omit_unretained_then_fail)
            && fault.phase >= 2
        {
            Some("injected process-table failure after omission".to_string())
        } else {
            None
        }
    };
    let mut processes = match injected_scan_error.map_or_else(linux_process_table, Err) {
        Ok(processes) => processes,
        Err(error) => {
            return Err(match &direct_inventory {
                Ok(true) => error,
                Ok(false) => {
                    format!("{error}; direct child process inventory is unavailable")
                }
                Err(direct) => format!("{error}; direct child process inventory failed: {direct}"),
            });
        }
    };
    {
        let mut fault = boundary
            .scan_fault
            .lock()
            .map_err(|_| "process-scan fault state was poisoned".to_string())?;
        if fault.omit_retained_then_fail && fault.phase == 0 {
            let retained = boundary
                .retained
                .lock()
                .map_err(|_| "retained process-boundary map was poisoned".to_string())?;
            let descendants = retained
                .keys()
                .copied()
                .filter(|key| *key != boundary.leader)
                .collect::<Vec<_>>();
            if !descendants.is_empty() {
                fault.omitted_pid = descendants.first().map(|key| key.pid);
                for key in descendants {
                    processes.remove(&key.pid);
                }
                fault.phase = 1;
            }
        } else if fault.omit_unretained_then_fail && fault.phase == 0 {
            let retained = boundary
                .retained
                .lock()
                .map_err(|_| "retained process-boundary map was poisoned".to_string())?;
            let omitted = processes.iter().find_map(|(&pid, identity)| {
                let key = ProcessKey {
                    pid,
                    start: identity.start,
                };
                (pid != boundary.leader.pid
                    && identity.live
                    && identity.parent == boundary.supervisor
                    && identity.start >= boundary.leader.start
                    && !boundary.baseline_direct.contains(&key)
                    && !retained.contains_key(&key))
                .then_some(pid)
            });
            if let Some(pid) = omitted {
                processes.remove(&pid);
                fault.phase = 1;
                fault.omitted_pid = Some(pid);
            }
        }
    }
    let mut owned = boundary
        .observed
        .lock()
        .map_err(|_| "process-boundary identity set was poisoned".to_string())?
        .clone();
    owned.insert(boundary.leader);
    loop {
        let before = owned.len();
        for (&pid, identity) in &processes {
            let key = ProcessKey {
                pid,
                start: identity.start,
            };
            let parent_owned = processes.get(&identity.parent).is_some_and(|parent| {
                owned.contains(&ProcessKey {
                    pid: identity.parent,
                    start: parent.start,
                })
            });
            let adopted_after_spawn = identity.parent == boundary.supervisor
                && identity.start >= boundary.leader.start
                && !boundary.baseline_direct.contains(&key);
            if identity.session == boundary.leader.pid || parent_owned || adopted_after_spawn {
                owned.insert(key);
            }
        }
        if owned.len() == before {
            break;
        }
    }
    *boundary
        .observed
        .lock()
        .map_err(|_| "process-boundary identity set was poisoned".to_string())? = owned.clone();

    let mut retained = boundary
        .retained
        .lock()
        .map_err(|_| "retained process-boundary map was poisoned".to_string())?;
    for key in owned {
        if retained.contains_key(&key) {
            continue;
        }
        let Some(identity) = processes.get(&key.pid) else {
            continue;
        };
        if identity.start != key.start || !identity.live {
            continue;
        }
        if let Some(member) = open_process_member(key.pid, identity)? {
            retained.insert(member.key, member.pidfd);
        }
    }
    let members = retained.keys().copied().collect();
    drop(retained);
    direct_inventory?;
    Ok(members)
}

#[cfg(target_os = "linux")]
fn signal_original_group(session: u32, signal: i32) -> Result<(), String> {
    // SAFETY: the benchmark created a fresh session whose positive leader PID
    // remains unreaped until this cleanup finishes.
    if unsafe { libc::kill(-(session as i32), signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(format!("signal process group {session}: {error}"))
    }
}

#[cfg(target_os = "linux")]
fn signal_members(boundary: &ProcessBoundary, signal: i32) -> Result<(), String> {
    let retained = boundary
        .retained
        .lock()
        .map_err(|_| "retained process-boundary map was poisoned".to_string())?;
    for (&key, pidfd) in retained.iter() {
        // SAFETY: pidfd_send_signal targets the exact process instance held by
        // the descriptor, even if its numeric PID has since been recycled.
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                pidfd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(format!("signal pidfd for {}: {error}", key.pid));
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn drain_boundary(
    boundary: &ProcessBoundary,
    signal: i32,
    deadline: Instant,
) -> Result<bool, String> {
    let mut empty_observations = 0;
    let mut first_error = None;
    loop {
        let members = match live_boundary_members(boundary) {
            Ok(members) => members,
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
                prune_retained_members(boundary)?
            }
        };
        if members.is_empty() {
            empty_observations += 1;
            if empty_observations == 2 {
                return first_error.map_or(Ok(true), Err);
            }
        } else {
            empty_observations = 0;
            if let Err(error) = signal_members(boundary, signal) {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        if Instant::now() >= deadline {
            return first_error.map_or(Ok(false), Err);
        }
        std::thread::sleep(PROCESS_POLL);
    }
}

#[cfg(target_os = "linux")]
fn stop_boundary(boundary: &ProcessBoundary) -> Result<(), String> {
    let session = boundary.leader.pid;
    let mut first_error = signal_original_group(session, libc::SIGTERM).err();
    match drain_boundary(boundary, libc::SIGTERM, Instant::now() + TERMINATION_GRACE) {
        Ok(true) if first_error.is_none() => return Ok(()),
        Ok(_) => {}
        Err(error) if first_error.is_none() => first_error = Some(error),
        Err(_) => {}
    }
    if let Err(error) = signal_original_group(session, libc::SIGKILL) {
        if first_error.is_none() {
            first_error = Some(error);
        }
    }
    let cleared = match drain_boundary(
        boundary,
        libc::SIGKILL,
        Instant::now() + FORCED_CLEANUP_GRACE,
    ) {
        Ok(cleared) => cleared,
        Err(error) => {
            if first_error.is_none() {
                first_error = Some(error);
            }
            false
        }
    };
    if let Some(error) = first_error {
        return Err(error);
    }
    if !cleared {
        return Err(format!(
            "session {session} retained live descendants after forced cleanup"
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn descendant_quiescence(
    boundary: &ProcessBoundary,
    command_deadline: Instant,
) -> Result<Option<Instant>, String> {
    let deadline = (Instant::now() + SUCCESS_QUIESCENCE_GRACE).min(command_deadline);
    let mut empty_observations = 0;
    loop {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        let has_descendant = live_boundary_members(boundary)?
            .iter()
            .any(|member| *member != boundary.leader);
        let observed_at = Instant::now();
        if observed_at >= deadline {
            return Ok(None);
        }
        if has_descendant {
            empty_observations = 0;
        } else {
            empty_observations += 1;
            if empty_observations == 2 {
                return Ok(Some(observed_at));
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        std::thread::sleep(PROCESS_POLL.min(remaining));
    }
}

#[cfg(target_os = "linux")]
fn reap_observed_children(boundary: &ProcessBoundary) -> Result<(), String> {
    let observed = boundary
        .observed
        .lock()
        .map_err(|_| "process-boundary identity set was poisoned".to_string())?
        .clone();
    for key in observed {
        if key == boundary.leader {
            continue;
        }
        let mut status = 0;
        // SAFETY: waitpid with WNOHANG observes only the exact adopted child;
        // non-children and already-reaped children report ECHILD and are done.
        let result = unsafe { libc::waitpid(key.pid as i32, &mut status, libc::WNOHANG) };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ECHILD) {
                return Err(format!("reap descendant {}: {error}", key.pid));
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn capture_size_error(path: &Path, stream: &str, limit: u64) -> Result<Option<String>, String> {
    let size = fs::metadata(path)
        .map_err(|error| format!("inspect command {stream}: {error}"))?
        .len();
    Ok((size > limit).then(|| format!("command {stream} exceeded {limit} bytes")))
}

#[cfg(target_os = "linux")]
fn read_bounded_capture(path: &Path, stream: &str, limit: u64) -> Result<Vec<u8>, String> {
    if let Some(error) = capture_size_error(path, stream, limit)? {
        return Err(error);
    }
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|error| format!("open command {stream}: {error}"))?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read command {stream}: {error}"))?;
    if bytes.len() as u64 > limit {
        return Err(format!("command {stream} exceeded {limit} bytes"));
    }
    Ok(bytes)
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct BoundedCapture {
    bytes: Vec<u8>,
    exceeded: bool,
    complete: bool,
}

#[cfg(target_os = "linux")]
fn drain_bounded_capture_with_signal<R: std::io::Read>(
    mut reader: R,
    limit: u64,
    exceeded_signal: Option<&AtomicBool>,
) -> std::io::Result<BoundedCapture> {
    let limit = usize::try_from(limit)
        .map_err(|_| std::io::Error::other("capture limit exceeds addressable memory"))?;
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut buffer = [0_u8; 64 * 1024];
    let mut exceeded = false;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let retained = read.min(limit.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&buffer[..retained]);
        if retained != read {
            exceeded = true;
            if let Some(signal) = exceeded_signal {
                signal.store(true, Ordering::Release);
            }
        }
    }
    Ok(BoundedCapture {
        bytes,
        exceeded,
        complete: true,
    })
}

#[cfg(target_os = "linux")]
fn drain_bounded_capture<R: std::io::Read>(
    reader: R,
    limit: u64,
) -> std::io::Result<BoundedCapture> {
    drain_bounded_capture_with_signal(reader, limit, None)
}

#[cfg(target_os = "linux")]
fn drain_bounded_pipe<R: std::io::Read + std::os::fd::AsRawFd>(
    mut reader: R,
    limit: u64,
    exceeded_signal: &AtomicBool,
    stop_signal: &AtomicBool,
) -> std::io::Result<BoundedCapture> {
    let limit = usize::try_from(limit)
        .map_err(|_| std::io::Error::other("capture limit exceeds addressable memory"))?;
    let descriptor = reader.as_raw_fd();
    // SAFETY: fcntl reads and updates only the flags on this owned pipe end.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut buffer = [0_u8; 64 * 1024];
    let mut exceeded = false;
    loop {
        if stop_signal.load(Ordering::Acquire) {
            return Ok(BoundedCapture {
                bytes,
                exceeded,
                complete: false,
            });
        }
        match reader.read(&mut buffer) {
            Ok(0) => {
                return Ok(BoundedCapture {
                    bytes,
                    exceeded,
                    complete: true,
                });
            }
            Ok(read) => {
                let retained = read.min(limit.saturating_sub(bytes.len()));
                bytes.extend_from_slice(&buffer[..retained]);
                if retained != read {
                    exceeded = true;
                    exceeded_signal.store(true, Ordering::Release);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let mut pollfd = libc::pollfd {
                    fd: descriptor,
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: pollfd points to one initialized descriptor record.
                let result = unsafe { libc::poll(&mut pollfd, 1, 10) };
                if result < 0
                    && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                {
                    return Err(std::io::Error::last_os_error());
                }
                if pollfd.revents & libc::POLLNVAL != 0 {
                    return Err(std::io::Error::other("capture pipe became invalid"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

#[cfg(target_os = "linux")]
fn supervise_timed_command(
    command: &mut Command,
    timeout: Duration,
    capture_limit: u64,
) -> Result<TimedOutput, String> {
    ensure_child_subreaper()?;
    let supervisor = std::process::id();
    let baseline_direct = match direct_child_keys(supervisor)? {
        Some(children) => children,
        None => linux_process_table()?
            .into_iter()
            .filter_map(|(pid, identity)| {
                (identity.parent == supervisor).then_some(ProcessKey {
                    pid,
                    start: identity.start,
                })
            })
            .collect(),
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: setsid has no memory arguments and is async-signal-safe between
    // fork and exec. It gives the harness an owned descendant boundary.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let start = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn command: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "capture command stdout pipe".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "capture command stderr pipe".to_string())?;
    let stdout_exceeded = Arc::new(AtomicBool::new(false));
    let stderr_exceeded = Arc::new(AtomicBool::new(false));
    let capture_stop = Arc::new(AtomicBool::new(false));
    let stdout_signal = Arc::clone(&stdout_exceeded);
    let stderr_signal = Arc::clone(&stderr_exceeded);
    let stdout_stop = Arc::clone(&capture_stop);
    let stderr_stop = Arc::clone(&capture_stop);
    let stdout_reader = std::thread::spawn(move || {
        drain_bounded_pipe(stdout, capture_limit, &stdout_signal, &stdout_stop)
    });
    let stderr_reader = std::thread::spawn(move || {
        drain_bounded_pipe(stderr, capture_limit, &stderr_signal, &stderr_stop)
    });
    let session = child.id();
    let boundary = match process_boundary(session, baseline_direct) {
        Ok(boundary) => boundary,
        Err(error) => {
            let _ = signal_original_group(session, libc::SIGKILL);
            let _ = child.wait();
            capture_stop.store(true, Ordering::Release);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(error);
        }
    };
    let (exit_tx, exit_rx) = mpsc::sync_channel(1);
    let waiter = std::thread::spawn(move || {
        let _ = exit_tx.send(wait_without_reaping(session));
    });

    let deadline = start + timeout;
    let (observed, capture_error) = loop {
        if stdout_exceeded.load(Ordering::Acquire) {
            break (
                None,
                Some(format!("command stdout exceeded {capture_limit} bytes")),
            );
        }
        if stderr_exceeded.load(Ordering::Acquire) {
            break (
                None,
                Some(format!("command stderr exceeded {capture_limit} bytes")),
            );
        }
        let now = Instant::now();
        if now >= deadline {
            break (None, None);
        }
        let wait = PROCESS_POLL.min(deadline.saturating_duration_since(now));
        match exit_rx.recv_timeout(wait) {
            Ok(result) => break (Some(Ok(result)), None),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break (Some(Err(mpsc::RecvTimeoutError::Disconnected)), None);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    let timed_out = observed.is_none() && capture_error.is_none();
    let exited_cleanly = observed
        .as_ref()
        .is_some_and(|result| matches!(result, Ok(Ok(()))));
    let wait_error = match observed.as_ref() {
        Some(Ok(Err(error))) => Some(format!("wait for command: {error}")),
        Some(Err(mpsc::RecvTimeoutError::Disconnected)) => {
            Some("command waiter disconnected".to_string())
        }
        _ => None,
    };
    let quiescence = if exited_cleanly {
        descendant_quiescence(&boundary, deadline)
    } else {
        Ok(None)
    };
    let cleanup = stop_boundary(&boundary);
    let status = child
        .wait()
        .map_err(|error| format!("reap command: {error}"));
    let waiter = waiter
        .join()
        .map_err(|_| "command waiter panicked".to_string());
    let reap = reap_observed_children(&boundary);
    if cleanup.is_err()
        || quiescence.is_err()
        || wait_error.is_some()
        || timed_out
        || capture_error.is_some()
        || status.is_err()
        || waiter.is_err()
    {
        // A total process-observation failure can leave an unknown writer. Do
        // not let that writer hold these reader joins indefinitely; the run is
        // already rejected, and the outer driver retains lifecycle authority.
        capture_stop.store(true, Ordering::Release);
    }
    let stdout = stdout_reader
        .join()
        .map_err(|_| "command stdout reader panicked".to_string())?
        .map_err(|error| format!("read command stdout: {error}"))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "command stderr reader panicked".to_string())?
        .map_err(|error| format!("read command stderr: {error}"))?;
    if let Some(error) = wait_error {
        return Err(error);
    }
    if timed_out {
        return Err(format!(
            "command timed out after {}s",
            timeout.as_secs_f64()
        ));
    }
    if let Err(error) = &quiescence {
        return Err(match &cleanup {
            Ok(()) => error.clone(),
            Err(cleanup) => format!("{error}; cleanup failed: {cleanup}"),
        });
    }
    let status = status?;
    waiter?;
    cleanup?;
    reap?;
    if let Some(error) = capture_error {
        return Err(error);
    }
    if stdout.exceeded {
        return Err(format!("command stdout exceeded {capture_limit} bytes"));
    }
    if stderr.exceeded {
        return Err(format!("command stderr exceeded {capture_limit} bytes"));
    }
    if !stdout.complete || !stderr.complete {
        return Err("command capture did not reach end of stream".to_string());
    }
    let quiescent_at = quiescence
        .expect("quiescence errors returned above")
        .ok_or_else(|| "command exited with a live descendant".to_string())?;
    Ok(TimedOutput {
        elapsed_ns: quiescent_at.saturating_duration_since(start).as_nanos(),
        output: Output {
            status,
            stdout: stdout.bytes,
            stderr: stderr.bytes,
        },
    })
}

#[cfg(not(target_os = "linux"))]
fn supervise_timed_command(
    _command: &mut Command,
    _timeout: Duration,
    _capture_limit: u64,
) -> Result<TimedOutput, String> {
    Err("the release performance gate requires Linux process supervision".to_string())
}

#[cfg(target_os = "linux")]
fn write_records(path: &Path, records: &[Vec<u8>]) -> Result<(), String> {
    let encoded_len = records.iter().try_fold(0_u64, |total, record| {
        if record.len() > MAX_SUPERVISOR_RECORD_BYTES {
            return Err("supervisor input record is too large".to_string());
        }
        total
            .checked_add(8)
            .and_then(|total| total.checked_add(record.len() as u64))
            .filter(|total| *total <= MAX_SUPERVISOR_RECORD_FILE_BYTES)
            .ok_or_else(|| "supervisor input total is too large".to_string())
    })?;
    debug_assert!(encoded_len <= MAX_SUPERVISOR_RECORD_FILE_BYTES);
    let mut file = BufWriter::new(
        File::create(path).map_err(|error| format!("create supervisor input: {error}"))?,
    );
    for record in records {
        file.write_all(&(record.len() as u64).to_le_bytes())
            .map_err(|error| format!("write supervisor input length: {error}"))?;
        file.write_all(record)
            .map_err(|error| format!("write supervisor input: {error}"))?;
    }
    file.flush()
        .map_err(|error| format!("flush supervisor input: {error}"))
}

#[cfg(target_os = "linux")]
fn read_records(path: &Path) -> Result<Vec<Vec<u8>>, String> {
    let mut file = File::open(path).map_err(|error| format!("open supervisor input: {error}"))?;
    if file
        .metadata()
        .map_err(|error| format!("inspect supervisor input: {error}"))?
        .len()
        > MAX_SUPERVISOR_RECORD_FILE_BYTES
    {
        return Err("supervisor input total is too large".to_string());
    }
    let mut records = Vec::new();
    loop {
        let mut length = [0; 8];
        match file.read(&mut length[..1]) {
            Ok(0) => break,
            Ok(1) => file
                .read_exact(&mut length[1..])
                .map_err(|error| format!("read supervisor input length: {error}"))?,
            Ok(_) => unreachable!("one-byte read returned more than one byte"),
            Err(error) => return Err(format!("read supervisor input length: {error}")),
        }
        let length = usize::try_from(u64::from_le_bytes(length))
            .ok()
            .filter(|length| *length <= MAX_SUPERVISOR_RECORD_BYTES)
            .ok_or_else(|| "supervisor input record is too large".to_string())?;
        let mut record = vec![0; length];
        file.read_exact(&mut record)
            .map_err(|error| format!("read supervisor input: {error}"))?;
        records.push(record);
    }
    Ok(records)
}

#[cfg(target_os = "linux")]
fn write_command_spec(command: &Command, directory: &Path) -> Result<(), String> {
    fs::write(directory.join("program"), command.get_program().as_bytes())
        .map_err(|error| format!("write supervisor program: {error}"))?;
    if let Some(cwd) = command.get_current_dir() {
        fs::write(directory.join("cwd"), cwd.as_os_str().as_bytes())
            .map_err(|error| format!("write supervisor cwd: {error}"))?;
    }
    let args = command
        .get_args()
        .map(|argument| argument.as_bytes().to_vec())
        .collect::<Vec<_>>();
    write_records(&directory.join("args"), &args)?;
    let mut environment = Vec::new();
    for (key, value) in command.get_envs() {
        let value = value
            .ok_or_else(|| "timed commands must use an explicit cleared environment".to_string())?;
        environment.push(key.as_bytes().to_vec());
        environment.push(value.as_bytes().to_vec());
    }
    write_records(&directory.join("environment"), &environment)
}

#[cfg(target_os = "linux")]
fn command_from_spec(directory: &Path) -> Result<Command, String> {
    let program = fs::read(directory.join("program"))
        .map_err(|error| format!("read supervisor program: {error}"))?;
    let mut command = Command::new(OsString::from_vec(program));
    command.env_clear();
    for argument in read_records(&directory.join("args"))? {
        command.arg(OsString::from_vec(argument));
    }
    let environment = read_records(&directory.join("environment"))?;
    if environment.len() % 2 != 0 {
        return Err("supervisor environment record is incomplete".to_string());
    }
    for pair in environment.chunks_exact(2) {
        command.env(
            OsString::from_vec(pair[0].clone()),
            OsString::from_vec(pair[1].clone()),
        );
    }
    let cwd = directory.join("cwd");
    if cwd.exists() {
        command.current_dir(OsString::from_vec(
            fs::read(cwd).map_err(|error| format!("read supervisor cwd: {error}"))?,
        ));
    }
    Ok(command)
}

#[cfg(target_os = "linux")]
fn run_timed_command_with_capture_limit(
    command: &mut Command,
    timeout: Duration,
    capture_limit: u64,
) -> Result<TimedOutput, String> {
    run_timed_command_with_scan_fault_marker(command, timeout, capture_limit, None, None)
}

#[cfg(target_os = "linux")]
fn run_timed_command_with_scan_fault(
    command: &mut Command,
    timeout: Duration,
    capture_limit: u64,
    scan_fault: Option<&str>,
) -> Result<TimedOutput, String> {
    run_timed_command_with_scan_fault_marker(command, timeout, capture_limit, scan_fault, None)
}

#[cfg(target_os = "linux")]
fn run_timed_command_with_scan_fault_marker(
    command: &mut Command,
    timeout: Duration,
    capture_limit: u64,
    scan_fault: Option<&str>,
    direct_pid_file: Option<&Path>,
) -> Result<TimedOutput, String> {
    let exchange = Scratch::new("perf-supervisor-exchange")
        .map_err(|error| format!("create supervisor exchange: {error}"))?;
    write_command_spec(command, exchange.path())?;
    let current = std::env::current_exe()
        .map_err(|error| format!("resolve performance test executable: {error}"))?;
    let mut supervisor = Command::new(current);
    supervisor
        .args([
            "--ignored",
            "--exact",
            "performance_supervisor_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env_clear()
        .env(SUPERVISOR_SPEC_ENV, exchange.path())
        .env(
            "DOT_PERF_SUPERVISOR_TIMEOUT_NS",
            timeout.as_nanos().to_string(),
        )
        .env(
            "DOT_PERF_SUPERVISOR_CAPTURE_BYTES",
            capture_limit.to_string(),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(scan_fault) = scan_fault {
        supervisor.env(SUPERVISOR_SCAN_FAULT_ENV, scan_fault);
    }
    if let Some(path) = direct_pid_file {
        supervisor.env(SUPERVISOR_DIRECT_PID_FILE_ENV, path);
    }
    let status = supervisor
        .status()
        .map_err(|error| format!("spawn performance supervisor: {error}"))?;
    if !status.success() {
        let error = fs::read_to_string(exchange.path().join("error"))
            .unwrap_or_else(|_| "performance supervisor failed without a diagnostic".to_string());
        return Err(error);
    }
    let elapsed_ns = fs::read_to_string(exchange.path().join("elapsed-ns"))
        .map_err(|error| format!("read supervisor elapsed time: {error}"))?
        .parse::<u128>()
        .map_err(|error| format!("parse supervisor elapsed time: {error}"))?;
    let raw_status = fs::read_to_string(exchange.path().join("status"))
        .map_err(|error| format!("read supervisor status: {error}"))?
        .parse::<i32>()
        .map_err(|error| format!("parse supervisor status: {error}"))?;
    let stdout = read_bounded_capture(&exchange.path().join("stdout"), "stdout", capture_limit)?;
    let stderr = read_bounded_capture(&exchange.path().join("stderr"), "stderr", capture_limit)?;
    Ok(TimedOutput {
        elapsed_ns,
        output: Output {
            status: std::process::ExitStatus::from_raw(raw_status),
            stdout,
            stderr,
        },
    })
}

#[cfg(not(target_os = "linux"))]
fn run_timed_command_with_capture_limit(
    command: &mut Command,
    timeout: Duration,
    capture_limit: u64,
) -> Result<TimedOutput, String> {
    supervise_timed_command(command, timeout, capture_limit)
}

fn run_timed_command(command: &mut Command, timeout: Duration) -> Result<TimedOutput, String> {
    run_timed_command_with_capture_limit(command, timeout, CAPTURE_LIMIT_BYTES)
}

#[cfg(target_os = "linux")]
fn process_is_live(pid: u32) -> bool {
    fs::read(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            let delimiter = stat.windows(2).rposition(|window| window == b") ")?;
            stat.get(delimiter + 2).copied()
        })
        .is_some_and(|state| !matches!(state, b'Z' | b'X' | b'x'))
}

#[cfg(target_os = "linux")]
fn wait_for_process_marker(path: &Path, timeout: Duration) -> Result<u32, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(value) = fs::read_to_string(path) {
            if let Ok(pid) = value.trim().parse::<u32>() {
                return Ok(pid);
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("process marker was not ready: {}", path.display()));
        }
        std::thread::sleep(PROCESS_POLL);
    }
}

fn engine_command(engine: &Engine, tools: &PerfTools) -> Command {
    match engine.kind {
        EngineKind::Shell => {
            let mut command = Command::new(&tools.bash);
            command.arg(&engine.executable);
            command
        }
        EngineKind::Rust => Command::new(&engine.executable),
    }
}

fn run_dot_unchecked(
    engine: &Engine,
    client: &Client,
    tools: &PerfTools,
    args: &[&str],
) -> TimedOutput {
    let mut command = engine_command(engine, tools);
    client_env(&mut command, engine, client, tools);
    command.args(args);
    run_timed_command(&mut command, COMMAND_TIMEOUT)
        .unwrap_or_else(|error| panic!("run {}: {error}", engine.executable.display()))
}

fn run_dot(engine: &Engine, client: &Client, tools: &PerfTools, args: &[&str]) -> TimedOutput {
    let timed = run_dot_unchecked(engine, client, tools, args);
    assert!(
        timed.output.status.success(),
        "{} dot {args:?} failed: stdout={} stderr={}",
        engine.kind.label(),
        String::from_utf8_lossy(&timed.output.stdout),
        String::from_utf8_lossy(&timed.output.stderr)
    );
    timed
}

fn empty_client(scratch: &Scratch, tag: &str) -> Client {
    let client = Client {
        home: scratch.path().join(format!("home-{tag}")),
        state: scratch.path().join(format!("state-{tag}")),
        xdg: scratch.path().join(format!("xdg-{tag}")),
        data: scratch.path().join(format!("data-{tag}")),
        cache: scratch.path().join(format!("cache-{tag}")),
        tmp: scratch.path().join(format!("tmp-{tag}")),
        extra_env: Vec::new(),
    };
    fs::create_dir_all(&client.home).expect("create client home");
    fs::create_dir_all(&client.state).expect("create client state");
    fs::create_dir_all(&client.xdg).expect("create client config");
    fs::create_dir_all(&client.data).expect("create client data");
    fs::create_dir_all(&client.cache).expect("create client cache");
    fs::create_dir_all(&client.tmp).expect("create client temporary directory");
    client
}

fn seed_remote(
    tools: &PerfTools,
    scratch: &Scratch,
    namespace: &str,
    name: &str,
    branch: &str,
    prefix: &str,
    files: usize,
) -> PathBuf {
    let seed = scratch.path().join(format!("{namespace}-{name}-seed"));
    let root = seed.join(prefix);
    fs::create_dir_all(&root).expect("create seed directory");
    git(tools, &seed, &["init", "-q"]);
    git(tools, &seed, &["config", "user.name", "fixture"]);
    git(
        tools,
        &seed,
        &["config", "user.email", "fixture@example.invalid"],
    );
    for index in 0..files {
        let relative = format!("{prefix}{name}-file-{index:03}.txt");
        fs::write(seed.join(&relative), format!("{name} payload {index}\n"))
            .expect("write overlay payload");
        git(tools, &seed, &["add", &relative]);
    }
    git(tools, &seed, &["commit", "-qm", "seed"]);
    git(tools, &seed, &["branch", "-M", branch]);
    let origin = scratch.path().join(format!("{namespace}-{name}.git"));
    git(
        tools,
        scratch.path(),
        &[
            "clone",
            "-q",
            "--bare",
            &seed.to_string_lossy(),
            &origin.to_string_lossy(),
        ],
    );
    git(
        tools,
        &origin,
        &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")],
    );
    origin
}

#[derive(Debug)]
struct Remotes {
    overlays: Vec<PathBuf>,
    base: PathBuf,
    dirty_seed: PathBuf,
}

fn shared_remotes(tools: &PerfTools, scratch: &Scratch, namespace: &str) -> Remotes {
    let base_seed = scratch.path().join(format!("{namespace}-base-seed"));
    fs::create_dir_all(&base_seed).expect("create base seed");
    git(tools, &base_seed, &["init", "-q"]);
    git(tools, &base_seed, &["config", "user.name", "fixture"]);
    git(
        tools,
        &base_seed,
        &["config", "user.email", "fixture@example.invalid"],
    );
    fs::write(base_seed.join(".testrc"), b"base\n").expect("write base payload");
    let overlays = (0..OVERLAYS)
        .map(|index| {
            seed_remote(
                tools,
                scratch,
                namespace,
                &format!("overlay-{index}"),
                "main",
                "home/",
                FILES_PER_OVERLAY,
            )
        })
        .collect::<Vec<_>>();
    git(tools, &base_seed, &["add", "-A"]);
    git(tools, &base_seed, &["commit", "-qm", "seed"]);
    git(tools, &base_seed, &["branch", "-M", "main"]);
    let base = scratch.path().join(format!("{namespace}-base.git"));
    git(
        tools,
        scratch.path(),
        &[
            "clone",
            "-q",
            "--bare",
            &base_seed.to_string_lossy(),
            &base.to_string_lossy(),
        ],
    );
    git(tools, &base, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    Remotes {
        overlays,
        base,
        dirty_seed: scratch.path().join(format!("{namespace}-overlay-0-seed")),
    }
}

fn base_only_remotes(tools: &PerfTools, scratch: &Scratch) -> Remotes {
    let mut remotes = shared_remotes(tools, scratch, "base-only");
    remotes.overlays.clear();
    remotes
}

fn feature_remotes(tools: &PerfTools, scratch: &Scratch) -> Remotes {
    let mut remotes = shared_remotes(tools, scratch, "features");
    for index in 0..OVERLAYS {
        let seed = scratch
            .path()
            .join(format!("features-overlay-{index}-seed"));
        let relative = "home/shared.txt";
        fs::write(
            seed.join(relative),
            format!("overlay-{index} collision winner\n"),
        )
        .expect("write colliding payload");
        git(tools, &seed, &["add", relative]);
        git(tools, &seed, &["commit", "-qm", "add collision"]);
        git(
            tools,
            &seed,
            &[
                "push",
                "-q",
                &remotes.overlays[index].to_string_lossy(),
                "HEAD:main",
            ],
        );
    }
    remotes.overlays.push(seed_remote(
        tools,
        scratch,
        "features",
        "profile-excluded",
        "main",
        "home/",
        1,
    ));
    remotes.dirty_seed = scratch.path().join("features-overlay-2-seed");
    remotes
}

fn write_overlay_descriptors(client: &Client, remotes: &Remotes) {
    let descriptors = client.xdg.join("dot/overlays.d");
    fs::create_dir_all(&descriptors).expect("create overlay descriptors");
    for (index, origin) in remotes.overlays.iter().enumerate() {
        fs::write(
            descriptors.join(format!("overlay-{index}.conf")),
            format!("url=file://{}\n", origin.display()),
        )
        .expect("write overlay descriptor");
    }
}

fn initialized_client(
    tools: &PerfTools,
    scratch: &Scratch,
    tag: &str,
    engine: &Engine,
    remotes: &Remotes,
) -> Client {
    let client = empty_client(scratch, tag);
    write_overlay_descriptors(&client, remotes);
    let base_url = format!("file://{}", remotes.base.display());
    let _ = run_dot(engine, &client, tools, &["init", "--yes", &base_url]);
    client
}

fn feature_client(
    tools: &PerfTools,
    scratch: &Scratch,
    tag: &str,
    engine: &Engine,
    remotes: &Remotes,
    provider_root: &Path,
    provider_binary: &Path,
) -> Client {
    let mut client = empty_client(scratch, tag);
    write_overlay_descriptors(&client, remotes);
    let dot_config = client.xdg.join("dot");
    let profiles = dot_config.join("profiles.d");
    fs::create_dir_all(&profiles).expect("create profile directory");
    fs::write(
        profiles.join("base.conf"),
        b"version=1\noverlays=overlay-0,overlay-1,overlay-2\n",
    )
    .expect("write base profile");
    fs::write(
        dot_config.join("config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/extensions\n\
dependency_provider=shdeps\nshdeps_update_policy=pinned\ndefault_profile=base\n",
    )
    .expect("write feature config");

    let extensions = client.home.join("extensions");
    let pre_sync = extensions.join("pre-sync.d");
    let merge_hooks = extensions.join("merge-hooks.d");
    fs::create_dir_all(&pre_sync).expect("create pre-sync directory");
    fs::create_dir_all(&merge_hooks).expect("create merge-hook directory");
    let pre_sync_hook = pre_sync.join("10-prepare.sh");
    fs::write(
        &pre_sync_hook,
        b"prepare() { printf 'prepared\\n' >\"$HOME/pre-sync-output\"; }\n",
    )
    .expect("write pre-sync hook");
    let merge_hook = merge_hooks.join("10-managed.sh");
    fs::write(
        &merge_hook,
        b"merge() { block=$(dot_managed_block_build '# dot:benchmark' fixture merged) || return; dot_managed_block_merge \"$HOME/merged.conf\" \"$block\"; }\n",
    )
    .expect("write merge hook");
    for path in [&extensions, &pre_sync, &merge_hooks] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .expect("secure extension directory");
    }
    for path in [&pre_sync_hook, &merge_hook] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .expect("secure extension hook");
    }

    client.extra_env.extend([
        (
            OsString::from("SHDEPS_LIB"),
            provider_root.join("shdeps.sh").into_os_string(),
        ),
        (
            OsString::from("SHDEPS_RUST_CLI"),
            provider_binary.as_os_str().to_owned(),
        ),
        (
            OsString::from("SHDEPS_DIR"),
            client.data.join("shdeps").into_os_string(),
        ),
        (OsString::from("DOT_UPDATE_JOBS"), OsString::from("2")),
        (OsString::from("DOT_MERGE_JOBS"), OsString::from("2")),
    ]);
    let base_url = format!("file://{}", remotes.base.display());
    let _ = run_dot(engine, &client, tools, &["init", "--yes", &base_url]);
    client
}

fn failing_pre_sync_client(
    tools: &PerfTools,
    scratch: &Scratch,
    tag: &str,
    engine: &Engine,
    remotes: &Remotes,
) -> Client {
    let client = initialized_client(tools, scratch, tag, engine, remotes);
    let extensions = client.home.join("failure-extensions");
    let pre_sync = extensions.join("pre-sync.d");
    fs::create_dir_all(&pre_sync).expect("create failing pre-sync directory");
    let hook = pre_sync.join("10-fail.sh");
    fs::write(&hook, b"prepare() { return 7; }\n").expect("write failing pre-sync hook");
    fs::create_dir_all(client.xdg.join("dot")).expect("create failure config directory");
    fs::write(
        client.xdg.join("dot/config"),
        b"version=1\nextension_api=1\nextensions_dir=$HOME/failure-extensions\n\
dependency_provider=none\n",
    )
    .expect("write failure config");
    for path in [&extensions, &pre_sync] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .expect("secure failure extension directory");
    }
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700))
        .expect("secure failing pre-sync hook");
    client
}

fn should_skip(root: ClientRoot, relative: &Path) -> bool {
    let mut components = relative.components();
    let first = components.next();
    if matches!(root, ClientRoot::Home) {
        if let Some(Component::Normal(name)) = first {
            // `.scm.sqlite` is SCM's async telemetry database: a lingering
            // SCM helper may create it after Dot returns, so it is never
            // converged content (same exclusion as `tests/update_run.rs`).
            if name == OsStr::new(".dotfiles")
                || name == OsStr::new(".dot-backup")
                || name == OsStr::new(".scm.sqlite")
                || name.as_bytes().starts_with(b".dotfiles-overlay-")
            {
                return true;
            }
        }
    }
    if matches!(root, ClientRoot::State)
        && (relative == Path::new("dot/bash-v1")
            || relative == Path::new("shdeps/shdeps.self-update.stamp")
            || relative == Path::new("shdeps/.lock"))
    {
        return true;
    }
    matches!(root, ClientRoot::Data) && relative.starts_with(Path::new("shdeps/.git"))
}

/// Whether `full` holds no recorded content: every on-disk child is excluded
/// (recursively), or the directory is empty. Directory entries are
/// structural (same as `tests/cli.rs` `semantic_tree`, which never records
/// them); convergence is judged on files and symlinks. For example `dot/`
/// holding only the shell's `bash-v1` interpreter hint compares equal to a
/// missing or empty `dot/` on the native side, while any non-excluded file
/// under `dot/` keeps the directory and its content compared.
fn dir_holds_only_excluded(root: ClientRoot, full: &Path, home: &Path) -> bool {
    let children = match fs::read_dir(full) {
        Ok(children) => children,
        Err(_) => return false,
    };
    for child in children {
        let path = match child {
            Ok(child) => child.path(),
            Err(_) => return false,
        };
        let relative = match path.strip_prefix(home) {
            Ok(relative) => relative,
            Err(_) => return false,
        };
        if should_skip(root, relative) {
            continue;
        }
        let file_type = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata.file_type(),
            Err(_) => return false,
        };
        if file_type.is_dir() {
            if !dir_holds_only_excluded(root, &path, home) {
                return false;
            }
        } else {
            // A recorded file, symlink, or special entry keeps the directory.
            return false;
        }
    }
    true
}

fn normalize_state_value(root: Option<ClientRoot>, relative: &Path, value: Vec<u8>) -> Vec<u8> {
    if !matches!(root, Some(ClientRoot::State)) {
        return value;
    }
    if relative != Path::new("dot/init/completed") {
        return value;
    }
    const PRIVATE_FIELDS: [&[u8]; 8] = [
        b"git_dir",
        b"worktree",
        b"backup",
        b"dot",
        b"dot_revision",
        b"nonce",
        b"git_dev",
        b"git_ino",
    ];
    let mut normalized = Vec::with_capacity(value.len());
    for line in value.split_inclusive(|byte| *byte == b'\n') {
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        let Some(separator) = body.iter().position(|byte| *byte == b'=') else {
            normalized.extend_from_slice(line);
            continue;
        };
        if PRIVATE_FIELDS.contains(&&body[..separator]) {
            normalized.extend_from_slice(&body[..=separator]);
            normalized.extend_from_slice(b"$ENGINE_PRIVATE");
            if line.ends_with(b"\n") {
                normalized.push(b'\n');
            }
        } else {
            normalized.extend_from_slice(line);
        }
    }
    normalized
}

fn normalized_target(target: &Path, home: &Path) -> Vec<u8> {
    if let Ok(relative) = target.strip_prefix(home) {
        let mut value = b"$HOME".to_vec();
        if !relative.as_os_str().is_empty() {
            value.push(b'/');
            value.extend_from_slice(relative.as_os_str().as_bytes());
        }
        value
    } else {
        replace_bytes(
            target.as_os_str().as_bytes(),
            home.as_os_str().as_bytes(),
            b"$HOME",
        )
    }
}

fn snapshot_tree(home: &Path) -> Vec<TreeEntry> {
    snapshot_tree_with_filter(home, Some(ClientRoot::Home))
}

fn snapshot_complete_tree(root: &Path) -> Vec<TreeEntry> {
    snapshot_tree_with_filter(root, None)
}

fn snapshot_tree_with_filter(home: &Path, root: Option<ClientRoot>) -> Vec<TreeEntry> {
    let mut entries = Vec::new();
    let mut stack = vec![home.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory).expect("read snapshot directory") {
            let entry = entry.expect("read snapshot entry");
            let path = entry.path();
            let relative = path.strip_prefix(home).expect("snapshot path below home");
            if root.is_some_and(|root| should_skip(root, relative)) {
                continue;
            }
            let metadata = fs::symlink_metadata(&path).expect("snapshot metadata");
            let file_type = metadata.file_type();
            let (kind, value) = if file_type.is_dir() {
                if root.is_some_and(|root| dir_holds_only_excluded(root, &path, home)) {
                    continue;
                }
                stack.push(path.clone());
                (b'd', Vec::new())
            } else if file_type.is_file() {
                (
                    b'f',
                    normalize_state_value(
                        root,
                        relative,
                        fs::read(&path).expect("read snapshot file"),
                    ),
                )
            } else if file_type.is_symlink() {
                let target = fs::read_link(&path).expect("read snapshot symlink");
                (b'l', normalized_target(&target, home))
            } else {
                panic!("unsupported fixture entry: {}", path.display());
            };
            entries.push(TreeEntry {
                path: relative.as_os_str().as_bytes().to_vec(),
                kind,
                mode: metadata.permissions().mode() & 0o7777,
                value,
            });
        }
    }
    entries.sort();
    entries
}

fn snapshot_client(client: &Client) -> ClientSnapshot {
    let mut snapshot = [
        snapshot_tree_with_filter(&client.home, Some(ClientRoot::Home)),
        snapshot_tree_with_filter(&client.state, Some(ClientRoot::State)),
        snapshot_tree_with_filter(&client.xdg, Some(ClientRoot::Config)),
        snapshot_tree_with_filter(&client.data, Some(ClientRoot::Data)),
        snapshot_tree_with_filter(&client.cache, Some(ClientRoot::Cache)),
        snapshot_tree_with_filter(&client.tmp, Some(ClientRoot::Temporary)),
    ];
    for entries in &mut snapshot {
        for entry in entries {
            for (path, token) in [
                (&client.home, b"$HOME".as_slice()),
                (&client.state, b"$STATE".as_slice()),
                (&client.xdg, b"$CONFIG".as_slice()),
                (&client.data, b"$DATA".as_slice()),
                (&client.cache, b"$CACHE".as_slice()),
                (&client.tmp, b"$TMPDIR".as_slice()),
            ] {
                entry.value = replace_bytes(&entry.value, path.as_os_str().as_bytes(), token);
            }
        }
    }
    snapshot
}

fn snapshot_complete_client(client: &Client) -> ClientSnapshot {
    [
        snapshot_complete_tree(&client.home),
        snapshot_complete_tree(&client.state),
        snapshot_complete_tree(&client.xdg),
        snapshot_complete_tree(&client.data),
        snapshot_complete_tree(&client.cache),
        snapshot_complete_tree(&client.tmp),
    ]
}

fn validate_unchanged_client_state(
    client: &Client,
    expected: &ClientSnapshot,
) -> Result<(), String> {
    let actual = snapshot_client(client);
    for (index, root) in ClientRoot::ALL.into_iter().enumerate() {
        if actual[index] != expected[index] {
            return Err(format!("{} changed", root.label()));
        }
    }
    Ok(())
}

fn validate_paired_client_state(
    clients: &Clients,
    steady_shell: &ClientSnapshot,
    steady_rust: &ClientSnapshot,
) -> Result<(), String> {
    validate_unchanged_client_state(&clients.shell, steady_shell)
        .map_err(|error| format!("shell client state: {error}"))?;
    validate_unchanged_client_state(&clients.rust, steady_rust)
        .map_err(|error| format!("rust client state: {error}"))?;
    let shell = snapshot_client(&clients.shell);
    let rust = snapshot_client(&clients.rust);
    if shell != rust {
        return Err(format!(
            "client state parity changed: {}",
            differing_client_entries(&shell, &rust)
        ));
    }
    Ok(())
}

fn differing_client_entries(left: &ClientSnapshot, right: &ClientSnapshot) -> String {
    let mut differences = Vec::new();
    for (index, root) in ClientRoot::ALL.into_iter().enumerate() {
        for entry in left[index].iter().chain(&right[index]) {
            if !left[index].contains(entry) || !right[index].contains(entry) {
                let path = String::from_utf8_lossy(&entry.path);
                differences.push(format!("{}:{path}", root.label()));
            }
        }
    }
    differences.sort();
    differences.dedup();
    differences.join(",")
}

fn snapshot_optional_tree(root: &Path) -> Vec<TreeEntry> {
    if root.exists() {
        snapshot_complete_tree(root)
    } else {
        Vec::new()
    }
}

fn snapshot_immutable_inputs(client: &Client) -> ImmutableInputs {
    [
        snapshot_complete_tree(&client.xdg),
        snapshot_optional_tree(&client.home.join("extensions")),
        snapshot_optional_tree(&client.home.join("failure-extensions")),
    ]
}

fn validate_immutable_inputs(client: &Client, expected: &ImmutableInputs) -> Result<(), String> {
    if snapshot_immutable_inputs(client) == *expected {
        Ok(())
    } else {
        Err("configuration or extension inputs changed".to_string())
    }
}

fn replace_bytes(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() {
        return bytes.to_vec();
    }
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(from) {
            output.extend_from_slice(to);
            index += from.len();
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    output
}

fn normalize_timing(bytes: &[u8]) -> Vec<u8> {
    dot::progress_ui::normalize_elapsed(bytes)
}

fn normalize_output(bytes: &[u8], engine: &Engine, client: &Client) -> Vec<u8> {
    let mut normalized = bytes.to_vec();
    for (path, token) in [
        (&client.home, b"$HOME".as_slice()),
        (&client.state, b"$STATE".as_slice()),
        (&client.xdg, b"$XDG".as_slice()),
        (&client.data, b"$DATA".as_slice()),
        (&client.cache, b"$CACHE".as_slice()),
        (&client.tmp, b"$TMPDIR".as_slice()),
        (&engine.source_root, b"$SOURCE".as_slice()),
    ] {
        normalized = replace_bytes(&normalized, path.as_os_str().as_bytes(), token);
    }
    normalized = replace_bytes(&normalized, engine.short_commit().as_bytes(), b"$COMMIT");
    normalize_timing(&normalized)
}

fn validate_first_spawn(
    output: &Output,
    expected_stdout: &[u8],
    before: &ClientSnapshot,
    after: &ClientSnapshot,
) -> Result<(), String> {
    if !output.status.success() {
        return Err(format!("first spawn exited with {}", output.status));
    }
    if output.stdout != expected_stdout {
        return Err("first-spawn stdout differed from the shell contract".to_string());
    }
    if !output.stderr.is_empty() {
        return Err("first-spawn stderr was not empty".to_string());
    }
    if before != after {
        return Err("first spawn changed HOME, an XDG root, or TMPDIR".to_string());
    }
    Ok(())
}

fn compare_outputs(
    workload: Workload,
    engines: &Engines,
    clients: &Clients,
    shell: &TimedOutput,
    rust: &TimedOutput,
) {
    assert_eq!(
        normalize_output(&shell.output.stdout, &engines.shell, &clients.shell),
        normalize_output(&rust.output.stdout, &engines.rust, &clients.rust),
        "{} stdout parity",
        workload.label()
    );
    assert_eq!(
        normalize_output(&shell.output.stderr, &engines.shell, &clients.shell),
        normalize_output(&rust.output.stderr, &engines.rust, &clients.rust),
        "{} stderr parity",
        workload.label()
    );
}

fn paired_startup(
    workload: Workload,
    args: &[&str],
    engines: &Engines,
    clients: &Clients,
    tools: &PerfTools,
    artifacts: &mut Artifacts,
) -> Measurements {
    let policy = workload.policy();
    let steady_shell = snapshot_client(&clients.shell);
    let steady_rust = snapshot_client(&clients.rust);
    assert!(
        steady_shell == steady_rust,
        "{} startup fixture state parity failed in {}",
        workload.label(),
        differing_client_entries(&steady_shell, &steady_rust)
    );
    let warmup_schedule = startup_warmup_schedule(workload);
    assert_eq!(warmup_schedule.len(), policy.warmups_per_engine);
    for order in warmup_schedule.into_iter().skip(STARTUP_PREFLIGHT_PAIRS) {
        for kind in order {
            let _ = run_dot(engines.get(kind), clients.get(kind), tools, args);
            validate_unchanged_client_state(
                clients.get(kind),
                if kind == EngineKind::Shell {
                    &steady_shell
                } else {
                    &steady_rust
                },
            )
            .unwrap_or_else(|error| {
                panic!(
                    "{} {} warm-up changed state: {error}",
                    kind.label(),
                    workload.label()
                )
            });
        }
    }
    let mut measurements = Measurements::default();
    for iteration in 0..policy.samples_per_engine {
        let order = pair_order(iteration);
        let mut shell = None;
        let mut rust = None;
        for kind in order {
            let timed = run_dot(engines.get(kind), clients.get(kind), tools, args);
            match kind {
                EngineKind::Shell => shell = Some(timed),
                EngineKind::Rust => rust = Some(timed),
            }
        }
        let shell = shell.expect("shell startup sample");
        let rust = rust.expect("rust startup sample");
        compare_outputs(workload, engines, clients, &shell, &rust);
        validate_paired_client_state(clients, &steady_shell, &steady_rust).unwrap_or_else(
            |error| {
                panic!(
                    "{} startup sample {} state parity failed: {error}",
                    workload.label(),
                    iteration + 1
                )
            },
        );
        measurements.push(EngineKind::Shell, shell.elapsed_ns);
        measurements.push(EngineKind::Rust, rust.elapsed_ns);
        artifacts.record_pair(
            workload,
            iteration,
            order,
            shell.elapsed_ns,
            rust.elapsed_ns,
            0,
        );
    }
    measurements
}

fn validate_base_payload(home: &Path) -> Result<(), String> {
    let base = home.join(".testrc");
    let actual = fs::read(&base).map_err(|error| format!("read {}: {error}", base.display()))?;
    if actual != b"base\n" {
        return Err(format!("{} carries the wrong bytes", base.display()));
    }
    Ok(())
}

fn validate_payloads(home: &Path, dirty: Option<&[u8]>) -> Result<(), String> {
    validate_base_payload(home)?;
    for overlay in 0..OVERLAYS {
        for index in 0..FILES_PER_OVERLAY {
            let relative = format!("overlay-{overlay}-file-{index:03}.txt");
            let expected = if overlay == 0 && index == 0 {
                dirty
                    .map(<[u8]>::to_vec)
                    .unwrap_or_else(|| format!("overlay-{overlay} payload {index}\n").into_bytes())
            } else {
                format!("overlay-{overlay} payload {index}\n").into_bytes()
            };
            let path = home.join(&relative);
            let actual =
                fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
            if actual != expected {
                return Err(format!("{} carries the wrong bytes", path.display()));
            }
        }
    }
    Ok(())
}

fn validate_feature_payloads(home: &Path, provider_binary: &Path) -> Result<(), String> {
    validate_payloads(home, None)?;
    for (relative, expected) in [
        ("shared.txt", b"overlay-2 collision winner\n".as_slice()),
        ("pre-sync-output", b"prepared\n".as_slice()),
    ] {
        let path = home.join(relative);
        let actual =
            fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?;
        if actual != expected {
            return Err(format!("{} carries the wrong bytes", path.display()));
        }
    }
    let merged = home.join("merged.conf");
    let merged_bytes =
        fs::read(&merged).map_err(|error| format!("read {}: {error}", merged.display()))?;
    if !merged_bytes
        .windows(b"# dot:benchmark begin".len())
        .any(|window| window == b"# dot:benchmark begin")
        || !merged_bytes
            .windows(b"merged".len())
            .any(|window| window == b"merged")
    {
        return Err(format!(
            "{} lacks the managed merge block",
            merged.display()
        ));
    }
    let provider = home.join(".local/bin/shdeps");
    if !provider.is_symlink() {
        return Err(format!(
            "{} is not the activated provider",
            provider.display()
        ));
    }
    if fs::canonicalize(&provider).ok() != fs::canonicalize(provider_binary).ok() {
        return Err(format!(
            "{} does not resolve to the pinned provider executable",
            provider.display()
        ));
    }
    if home.join("profile-excluded-file-000.txt").exists() {
        return Err("profile-excluded overlay unexpectedly became active".to_string());
    }
    Ok(())
}

fn warm_updates<F>(
    workload: Workload,
    engines: &Engines,
    clients: &Clients,
    tools: &PerfTools,
    validate: F,
) -> (ClientSnapshot, ClientSnapshot)
where
    F: Fn(&Path) -> Result<(), String>,
{
    let mut last_shell = None;
    let mut last_rust = None;
    let shell_inputs = snapshot_immutable_inputs(&clients.shell);
    let rust_inputs = snapshot_immutable_inputs(&clients.rust);
    for iteration in 0..workload.policy().warmups_per_engine {
        for kind in pair_order(iteration) {
            let timed = run_dot(engines.get(kind), clients.get(kind), tools, &["update"]);
            validate_immutable_inputs(
                clients.get(kind),
                if kind == EngineKind::Shell {
                    &shell_inputs
                } else {
                    &rust_inputs
                },
            )
            .unwrap_or_else(|error| panic!("{} warm-up: {error}", kind.label()));
            match kind {
                EngineKind::Shell => last_shell = Some(timed),
                EngineKind::Rust => last_rust = Some(timed),
            }
        }
    }
    let shell = last_shell.expect("shell update warm-up");
    let rust = last_rust.expect("rust update warm-up");
    compare_outputs(workload, engines, clients, &shell, &rust);
    validate(&clients.shell.home).expect("shell warm payloads");
    validate(&clients.rust.home).expect("Rust warm payloads");
    let shell_snapshot = snapshot_client(&clients.shell);
    let rust_snapshot = snapshot_client(&clients.rust);
    assert!(
        shell_snapshot == rust_snapshot,
        "{} warm client snapshot parity failed in {}",
        workload.label(),
        differing_client_entries(&shell_snapshot, &rust_snapshot)
    );
    (shell_snapshot, rust_snapshot)
}

fn clean_updates<F>(
    workload: Workload,
    engines: &Engines,
    clients: &Clients,
    tools: &PerfTools,
    steady: (&ClientSnapshot, &ClientSnapshot),
    artifacts: &mut Artifacts,
    validate: F,
) -> Measurements
where
    F: Fn(&Path) -> Result<(), String>,
{
    let policy = workload.policy();
    let mut measurements = Measurements::default();
    let shell_inputs = snapshot_immutable_inputs(&clients.shell);
    let rust_inputs = snapshot_immutable_inputs(&clients.rust);
    for iteration in 0..policy.samples_per_engine {
        let order = pair_order(iteration);
        let mut shell = None;
        let mut rust = None;
        let mut shell_snapshot = None;
        let mut rust_snapshot = None;
        for kind in order {
            let timed = run_dot(engines.get(kind), clients.get(kind), tools, &["update"]);
            validate(&clients.get(kind).home).unwrap_or_else(|error| {
                panic!(
                    "{} {} sample {} did not converge: {error}",
                    kind.label(),
                    workload.label(),
                    iteration + 1
                )
            });
            validate_immutable_inputs(
                clients.get(kind),
                if kind == EngineKind::Shell {
                    &shell_inputs
                } else {
                    &rust_inputs
                },
            )
            .unwrap_or_else(|error| {
                panic!(
                    "{} {} sample {} mutated inputs: {error}",
                    kind.label(),
                    workload.label(),
                    iteration + 1
                )
            });
            let snapshot = snapshot_client(clients.get(kind));
            match kind {
                EngineKind::Shell => {
                    assert!(
                        &snapshot == steady.0,
                        "shell clean sample mutated persistent state"
                    );
                    shell = Some(timed);
                    shell_snapshot = Some(snapshot);
                }
                EngineKind::Rust => {
                    assert!(
                        &snapshot == steady.1,
                        "Rust clean sample mutated persistent state"
                    );
                    rust = Some(timed);
                    rust_snapshot = Some(snapshot);
                }
            }
        }
        let shell = shell.expect("shell clean sample");
        let rust = rust.expect("rust clean sample");
        compare_outputs(workload, engines, clients, &shell, &rust);
        let shell_snapshot = shell_snapshot.expect("shell clean snapshot");
        let rust_snapshot = rust_snapshot.expect("rust clean snapshot");
        assert!(
            shell_snapshot == rust_snapshot,
            "{} clean sample snapshot parity failed in {}",
            workload.label(),
            differing_client_entries(&shell_snapshot, &rust_snapshot)
        );
        measurements.push(EngineKind::Shell, shell.elapsed_ns);
        measurements.push(EngineKind::Rust, rust.elapsed_ns);
        artifacts.record_pair(
            workload,
            iteration,
            order,
            shell.elapsed_ns,
            rust.elapsed_ns,
            0,
        );
    }
    measurements
}

fn push_dirty_change(tools: &PerfTools, remotes: &Remotes, iteration: usize) -> Vec<u8> {
    let relative = "home/overlay-0-file-000.txt";
    let expected = format!("overlay-0 payload change-{iteration:03}\n").into_bytes();
    fs::write(remotes.dirty_seed.join(relative), &expected).expect("write dirty payload");
    git(tools, &remotes.dirty_seed, &["add", relative]);
    git(
        tools,
        &remotes.dirty_seed,
        &["commit", "-qm", &format!("change-{iteration:03}")],
    );
    git(
        tools,
        &remotes.dirty_seed,
        &[
            "push",
            "-q",
            &remotes.overlays[0].to_string_lossy(),
            "HEAD:main",
        ],
    );
    expected
}

fn dirty_updates(
    engines: &Engines,
    clients: &Clients,
    tools: &PerfTools,
    remotes: &Remotes,
    artifacts: &mut Artifacts,
) -> Measurements {
    let policy = Workload::DisjointDirty.policy();
    let mut measurements = Measurements::default();
    let shell_inputs = snapshot_immutable_inputs(&clients.shell);
    let rust_inputs = snapshot_immutable_inputs(&clients.rust);
    for iteration in 0..policy.samples_per_engine {
        let expected = push_dirty_change(tools, remotes, iteration);
        let order = pair_order(iteration);
        let mut shell = None;
        let mut rust = None;
        let mut shell_snapshot = None;
        let mut rust_snapshot = None;
        for kind in order {
            let timed = run_dot(engines.get(kind), clients.get(kind), tools, &["update"]);
            let client = clients.get(kind);
            validate_payloads(&client.home, Some(&expected)).unwrap_or_else(|error| {
                panic!(
                    "{} dirty sample {} did not converge: {error}",
                    kind.label(),
                    iteration + 1
                )
            });
            validate_immutable_inputs(
                client,
                if kind == EngineKind::Shell {
                    &shell_inputs
                } else {
                    &rust_inputs
                },
            )
            .unwrap_or_else(|error| {
                panic!(
                    "{} dirty sample {} mutated inputs: {error}",
                    kind.label(),
                    iteration + 1
                )
            });
            let snapshot = snapshot_client(client);
            match kind {
                EngineKind::Shell => {
                    shell = Some(timed);
                    shell_snapshot = Some(snapshot);
                }
                EngineKind::Rust => {
                    rust = Some(timed);
                    rust_snapshot = Some(snapshot);
                }
            }
        }
        let shell = shell.expect("shell dirty sample");
        let rust = rust.expect("rust dirty sample");
        compare_outputs(Workload::DisjointDirty, engines, clients, &shell, &rust);
        let shell_snapshot = shell_snapshot.expect("shell dirty snapshot");
        let rust_snapshot = rust_snapshot.expect("rust dirty snapshot");
        assert!(
            shell_snapshot == rust_snapshot,
            "dirty sample snapshot parity failed in {}",
            differing_client_entries(&shell_snapshot, &rust_snapshot)
        );
        measurements.push(EngineKind::Shell, shell.elapsed_ns);
        measurements.push(EngineKind::Rust, rust.elapsed_ns);
        artifacts.record_pair(
            Workload::DisjointDirty,
            iteration,
            order,
            shell.elapsed_ns,
            rust.elapsed_ns,
            0,
        );
    }
    measurements
}

fn failed_updates(
    engines: &Engines,
    clients: &Clients,
    tools: &PerfTools,
    artifacts: &mut Artifacts,
) -> Measurements {
    let policy = Workload::PreSyncFailure.policy();
    let steady_shell = snapshot_client(&clients.shell);
    let steady_rust = snapshot_client(&clients.rust);
    validate_paired_client_state(clients, &steady_shell, &steady_rust)
        .unwrap_or_else(|error| panic!("failure fixture state parity failed: {error}"));
    for iteration in 0..policy.warmups_per_engine {
        for kind in pair_order(iteration) {
            let timed = run_dot_unchecked(engines.get(kind), clients.get(kind), tools, &["update"]);
            assert_eq!(
                timed.output.status.code(),
                Some(1),
                "{} failure warm-up status",
                kind.label()
            );
            assert!(
                snapshot_client(clients.get(kind))
                    == *if kind == EngineKind::Shell {
                        &steady_shell
                    } else {
                        &steady_rust
                    },
                "{} failure warm-up mutated state",
                kind.label()
            );
        }
        validate_paired_client_state(clients, &steady_shell, &steady_rust).unwrap_or_else(
            |error| {
                panic!(
                    "failure warm-up {} state parity failed: {error}",
                    iteration + 1
                )
            },
        );
    }

    let mut measurements = Measurements::default();
    for iteration in 0..policy.samples_per_engine {
        let order = pair_order(iteration);
        let mut shell = None;
        let mut rust = None;
        for kind in order {
            let timed = run_dot_unchecked(engines.get(kind), clients.get(kind), tools, &["update"]);
            assert_eq!(
                timed.output.status.code(),
                Some(1),
                "{} failure sample {} status",
                kind.label(),
                iteration + 1
            );
            assert!(
                snapshot_client(clients.get(kind))
                    == *if kind == EngineKind::Shell {
                        &steady_shell
                    } else {
                        &steady_rust
                    },
                "{} failure sample {} mutated state",
                kind.label(),
                iteration + 1
            );
            match kind {
                EngineKind::Shell => shell = Some(timed),
                EngineKind::Rust => rust = Some(timed),
            }
        }
        let shell = shell.expect("shell failure sample");
        let rust = rust.expect("Rust failure sample");
        validate_paired_client_state(clients, &steady_shell, &steady_rust).unwrap_or_else(
            |error| {
                panic!(
                    "failure sample {} state parity failed: {error}",
                    iteration + 1
                )
            },
        );
        compare_outputs(Workload::PreSyncFailure, engines, clients, &shell, &rust);
        measurements.push(EngineKind::Shell, shell.elapsed_ns);
        measurements.push(EngineKind::Rust, rust.elapsed_ns);
        artifacts.record_pair(
            Workload::PreSyncFailure,
            iteration,
            order,
            shell.elapsed_ns,
            rust.elapsed_ns,
            1,
        );
    }
    measurements
}

fn summary_row(workload: Workload, measurements: &Measurements) -> SummaryRow {
    let policy = workload.policy();
    let (shell, rust) = measurements.stats();
    let relative_passed = policy
        .max_rust_percent
        .is_none_or(|percent| meets_relative_gate(rust.median_ns, shell.median_ns, percent));
    SummaryRow {
        workload,
        samples: measurements.rust.len(),
        shell: Some(shell),
        rust,
        max_rust_percent: policy.max_rust_percent,
        rust_p95_budget_ns: policy.rust_p95_budget_ns,
        passed: measurements.shell.len() == policy.samples_per_engine
            && measurements.rust.len() == policy.samples_per_engine
            && rust.p95_ns <= policy.rust_p95_budget_ns
            && relative_passed,
    }
}

fn print_summary(rows: &[SummaryRow]) {
    for row in rows {
        let shell = row
            .shell
            .map(|stats| format!(" shell median={}ns p95={}ns", stats.median_ns, stats.p95_ns))
            .unwrap_or_default();
        eprintln!(
            "{}:{shell} rust median={}ns p95={}ns budget={}ns {}",
            row.workload.label(),
            row.rust.median_ns,
            row.rust.p95_ns,
            row.rust_p95_budget_ns,
            if row.passed { "PASS" } else { "FAIL" },
        );
    }
}

#[cfg(target_os = "linux")]
fn supervisor_process_main(directory: &Path) -> Result<(), String> {
    let timeout_ns = std::env::var("DOT_PERF_SUPERVISOR_TIMEOUT_NS")
        .map_err(|_| "supervisor timeout is missing".to_string())?
        .parse::<u64>()
        .map_err(|error| format!("parse supervisor timeout: {error}"))?;
    let capture_limit = std::env::var("DOT_PERF_SUPERVISOR_CAPTURE_BYTES")
        .map_err(|_| "supervisor capture limit is missing".to_string())?
        .parse::<u64>()
        .map_err(|error| format!("parse supervisor capture limit: {error}"))?;
    let mut command = command_from_spec(directory)?;
    let timed = supervise_timed_command(
        &mut command,
        Duration::from_nanos(timeout_ns),
        capture_limit,
    )?;
    fs::write(directory.join("elapsed-ns"), timed.elapsed_ns.to_string())
        .map_err(|error| format!("write supervisor elapsed time: {error}"))?;
    fs::write(
        directory.join("status"),
        timed.output.status.into_raw().to_string(),
    )
    .map_err(|error| format!("write supervisor status: {error}"))?;
    fs::write(directory.join("stdout"), timed.output.stdout)
        .map_err(|error| format!("write supervisor stdout: {error}"))?;
    fs::write(directory.join("stderr"), timed.output.stderr)
        .map_err(|error| format!("write supervisor stderr: {error}"))?;
    Ok(())
}

#[test]
#[ignore = "private subprocess entry point for run_timed_command"]
fn performance_supervisor_process() {
    #[cfg(target_os = "linux")]
    if let Some(directory) = std::env::var_os(SUPERVISOR_SPEC_ENV) {
        let directory = PathBuf::from(directory);
        if let Err(error) = supervisor_process_main(&directory) {
            let _ = fs::write(directory.join("error"), error);
            std::process::exit(1);
        }
        std::process::exit(0);
    }
}

#[test]
#[ignore = "scripts/benchmark-port.sh runs the dedicated release-only performance gate"]
fn release_performance_gate() {
    let artifact_dir = PathBuf::from(std::env::var_os(ARTIFACT_DIR_ENV).unwrap_or_else(|| {
        panic!("{ARTIFACT_DIR_ENV} is required; use scripts/benchmark-port.sh")
    }));
    let tools = PerfTools::from_env().expect("validated performance toolchain");
    let engines = engines(&tools);
    let (provider_root, provider_binary, provider_identity) = provider_root(&tools, &engines);
    let run_id = std::env::var(RUN_ID_ENV)
        .unwrap_or_else(|_| panic!("{RUN_ID_ENV} is required; use scripts/benchmark-port.sh"));
    let evidence = EvidenceIdentity::new(
        &run_id,
        &engines,
        &tools,
        &provider_root,
        &provider_binary,
        &provider_identity,
    );
    let mut artifacts = Artifacts::new(&artifact_dir, &run_id);
    let dirty = std::env::var("DOT_PERF_CURRENT_DIRTY").unwrap_or_else(|_| "unknown".into());
    let runner_image = std::env::var(RUNNER_IMAGE_ENV).unwrap_or_else(|_| {
        panic!("{RUNNER_IMAGE_ENV} is required; use scripts/benchmark-port.sh")
    });
    let scratch = Scratch::new("perf-gate").expect("create isolated performance scratch");
    artifacts.write_metadata(
        &engines,
        &tools,
        (&provider_root, &provider_identity),
        &evidence,
        MetadataContext {
            fixture_root: scratch.path(),
            dirty: &dirty,
            runner_image: &runner_image,
        },
    );
    let startup_clients = Clients {
        shell: empty_client(&scratch, "startup-shell"),
        rust: empty_client(&scratch, "startup-rust"),
    };

    // This shell/Rust pair is the first declared help warm-up. The Rust half is
    // also retained as the separately budgeted first-spawn observation.
    let help_schedule = startup_warmup_schedule(Workload::Help);
    let [help_shell_kind, help_rust_kind] = help_schedule[0];
    assert_eq!(help_shell_kind, EngineKind::Shell);
    assert_eq!(help_rust_kind, EngineKind::Rust);
    let shell_help = run_dot(
        engines.get(help_shell_kind),
        startup_clients.get(help_shell_kind),
        &tools,
        &["help"],
    );
    assert!(
        shell_help.output.stderr.is_empty(),
        "shell help reference emitted stderr"
    );
    // This is intentionally the first native child. It is a loose regression
    // ceiling, not a cold-page-cache claim.
    let first_state = snapshot_complete_client(startup_clients.get(help_rust_kind));
    let first_spawn = run_dot(
        engines.get(help_rust_kind),
        startup_clients.get(help_rust_kind),
        &tools,
        &["help"],
    );
    let after_first = snapshot_complete_client(startup_clients.get(help_rust_kind));
    artifacts
        .record_first_spawn(
            first_spawn.elapsed_ns,
            validate_first_spawn(
                &first_spawn.output,
                &shell_help.output.stdout,
                &first_state,
                &after_first,
            ),
        )
        .expect("record validated first spawn");

    // Semantic identity validation is the first declared version warm-up pair.
    for kind in startup_warmup_schedule(Workload::Version)
        .into_iter()
        .take(STARTUP_PREFLIGHT_PAIRS)
        .flatten()
    {
        let engine = engines.get(kind);
        let client = startup_clients.get(kind);
        let version = run_dot(engine, client, &tools, &["version"]);
        assert_eq!(
            version.output.stdout,
            engine.version_line(),
            "{} executable identity",
            kind.label()
        );
        assert!(version.output.stderr.is_empty(), "version stderr is empty");
    }

    let help = paired_startup(
        Workload::Help,
        &["help"],
        &engines,
        &startup_clients,
        &tools,
        &mut artifacts,
    );
    let version = paired_startup(
        Workload::Version,
        &["version"],
        &engines,
        &startup_clients,
        &tools,
        &mut artifacts,
    );

    let base_remotes = base_only_remotes(&tools, &scratch);
    let base_clients = Clients {
        shell: initialized_client(
            &tools,
            &scratch,
            "base-shell",
            &engines.shell,
            &base_remotes,
        ),
        rust: initialized_client(&tools, &scratch, "base-rust", &engines.rust, &base_remotes),
    };
    let (base_steady_shell, base_steady_rust) = warm_updates(
        Workload::BaseClean,
        &engines,
        &base_clients,
        &tools,
        validate_base_payload,
    );
    let base_clean = clean_updates(
        Workload::BaseClean,
        &engines,
        &base_clients,
        &tools,
        (&base_steady_shell, &base_steady_rust),
        &mut artifacts,
        validate_base_payload,
    );

    let remotes = shared_remotes(&tools, &scratch, "disjoint");
    let clients = Clients {
        shell: initialized_client(&tools, &scratch, "update-shell", &engines.shell, &remotes),
        rust: initialized_client(&tools, &scratch, "update-rust", &engines.rust, &remotes),
    };
    let (steady_shell, steady_rust) = warm_updates(
        Workload::DisjointClean,
        &engines,
        &clients,
        &tools,
        |home| validate_payloads(home, None),
    );
    let clean = clean_updates(
        Workload::DisjointClean,
        &engines,
        &clients,
        &tools,
        (&steady_shell, &steady_rust),
        &mut artifacts,
        |home| validate_payloads(home, None),
    );
    let dirty = dirty_updates(&engines, &clients, &tools, &remotes, &mut artifacts);

    let feature_remotes = feature_remotes(&tools, &scratch);
    let feature_clients = Clients {
        shell: feature_client(
            &tools,
            &scratch,
            "feature-shell",
            &engines.shell,
            &feature_remotes,
            &provider_root,
            &provider_binary,
        ),
        rust: feature_client(
            &tools,
            &scratch,
            "feature-rust",
            &engines.rust,
            &feature_remotes,
            &provider_root,
            &provider_binary,
        ),
    };
    let (feature_steady_shell, feature_steady_rust) = warm_updates(
        Workload::FeatureCollision,
        &engines,
        &feature_clients,
        &tools,
        |home| validate_feature_payloads(home, &provider_binary),
    );
    let feature = clean_updates(
        Workload::FeatureCollision,
        &engines,
        &feature_clients,
        &tools,
        (&feature_steady_shell, &feature_steady_rust),
        &mut artifacts,
        |home| validate_feature_payloads(home, &provider_binary),
    );

    let failure_clients = Clients {
        shell: failing_pre_sync_client(
            &tools,
            &scratch,
            "failure-shell",
            &engines.shell,
            &base_remotes,
        ),
        rust: failing_pre_sync_client(
            &tools,
            &scratch,
            "failure-rust",
            &engines.rust,
            &base_remotes,
        ),
    };
    let failure = failed_updates(&engines, &failure_clients, &tools, &mut artifacts);

    let first_stats = Stats {
        median_ns: first_spawn.elapsed_ns,
        p95_ns: first_spawn.elapsed_ns,
    };
    let first_policy = Workload::FirstSpawn.policy();
    let rows = vec![
        SummaryRow {
            workload: Workload::FirstSpawn,
            samples: 1,
            shell: None,
            rust: first_stats,
            max_rust_percent: first_policy.max_rust_percent,
            rust_p95_budget_ns: first_policy.rust_p95_budget_ns,
            passed: first_spawn.elapsed_ns <= first_policy.rust_p95_budget_ns,
        },
        summary_row(Workload::Help, &help),
        summary_row(Workload::Version, &version),
        summary_row(Workload::BaseClean, &base_clean),
        summary_row(Workload::DisjointClean, &clean),
        summary_row(Workload::DisjointDirty, &dirty),
        summary_row(Workload::FeatureCollision, &feature),
        summary_row(Workload::PreSyncFailure, &failure),
    ];
    assert_eq!(
        rows.iter()
            .map(|row| row.workload.label())
            .collect::<Vec<_>>(),
        REQUIRED_WORKLOADS,
        "performance gate workload inventory"
    );
    artifacts.write_results(&rows, &evidence, &tools);
    print_summary(&rows);
    let failures = rows
        .iter()
        .filter(|row| !row.passed)
        .map(|row| row.workload.label())
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty(),
        "release performance gates failed: {}",
        failures.join(", ")
    );
}

#[test]
fn performance_output_normalization_changes_only_timing_fields() {
    assert_eq!(
        normalize_timing(b"[1/2] Updating overlays    19s\nDone in 7s.\nDone with errors in 8s.\n"),
        b"[1/2] Updating overlays @ELAPSED@\nDone in Ns.\nDone with errors in Ns.\n"
    );
    assert_eq!(
        normalize_timing(b"retry in 19s; 3s timeout; version 42; 1 repos\n"),
        b"retry in 19s; 3s timeout; version 42; 1 repos\n"
    );
}

#[test]
fn performance_harness_matches_the_required_workload_inventory() {
    assert_eq!(
        WORKLOAD_POLICIES.map(|policy| policy.workload.label()),
        REQUIRED_WORKLOADS,
    );
}

#[test]
fn performance_path_normalization_handles_repeated_roots() {
    assert_eq!(
        replace_bytes(b"/tmp/home/a -> /tmp/home/b", b"/tmp/home", b"$HOME"),
        b"$HOME/a -> $HOME/b"
    );
}

#[test]
fn performance_snapshot_normalizes_client_owned_symlinks() {
    let scratch = Scratch::new("perf-snapshot").expect("scratch");
    let first = scratch.path().join("first");
    let second = scratch.path().join("second");
    for home in [&first, &second] {
        fs::create_dir_all(home.join("owned")).expect("create tree");
        fs::write(home.join("owned/file"), b"payload\n").expect("write payload");
        std::os::unix::fs::symlink(home.join("owned/file"), home.join("link"))
            .expect("create link");
    }

    assert_eq!(snapshot_tree(&first), snapshot_tree(&second));
}

#[test]
fn successful_state_validation_covers_every_persistent_client_root() {
    let scratch = Scratch::new("perf-persistent-roots").expect("scratch");
    let client = empty_client(&scratch, "client");
    let steady = snapshot_client(&client);

    for (label, root) in [
        ("HOME", &client.home),
        ("XDG_STATE_HOME", &client.state),
        ("XDG_CONFIG_HOME", &client.xdg),
        ("XDG_DATA_HOME", &client.data),
        ("XDG_CACHE_HOME", &client.cache),
    ] {
        let mutation = root.join("unexpected-mutation");
        fs::write(&mutation, label).expect("write persistent-root mutation");
        let error = validate_unchanged_client_state(&client, &steady)
            .expect_err("persistent-root mutation must be detected");
        assert!(error.contains(label), "unexpected error: {error}");
        fs::remove_file(mutation).expect("remove persistent-root mutation");
    }
}

#[test]
fn successful_state_exclusions_are_narrow_and_engine_private() {
    let scratch = Scratch::new("perf-private-state").expect("scratch");
    let client = empty_client(&scratch, "client");
    fs::create_dir_all(client.data.join("shdeps")).expect("provider state root");
    let steady = snapshot_client(&client);

    fs::create_dir_all(client.home.join(".dotfiles/private")).expect("private home state");
    fs::write(client.home.join(".dotfiles/private/state"), b"engine\n")
        .expect("write private home state");
    fs::create_dir_all(client.data.join("shdeps/.git/objects")).expect("private provider state");
    fs::write(client.data.join("shdeps/.git/objects/state"), b"engine\n")
        .expect("write private provider state");
    validate_unchanged_client_state(&client, &steady).expect("engine-private state is excluded");

    fs::write(client.data.join("shdeps/user-visible"), b"significant\n")
        .expect("write significant provider state");
    assert!(validate_unchanged_client_state(&client, &steady).is_err());
}

#[test]
fn state_snapshot_normalizes_only_documented_engine_private_receipt_fields() {
    let scratch = Scratch::new("perf-state-receipt").expect("scratch");
    let shell = empty_client(&scratch, "shell");
    let rust = empty_client(&scratch, "rust");
    for client in [&shell, &rust] {
        fs::create_dir_all(client.state.join("dot/init")).expect("state receipt directory");
        fs::create_dir_all(client.state.join("shdeps")).expect("provider state directory");
    }
    fs::write(shell.state.join("dot/bash-v1"), b"shell runtime marker\n").expect("shell marker");
    fs::write(
        shell.state.join("dot/init/completed"),
        b"phase=complete\norigin=fixture\nbranch=main\ncommit=abc\nworktree=/private/shell\ndot=/private/shell-dot\ndot_revision=111\nnonce=111.222\ngit_dev=1\ngit_ino=2\n",
    )
    .expect("shell receipt");
    fs::write(
        rust.state.join("dot/init/completed"),
        b"phase=complete\norigin=fixture\nbranch=main\ncommit=abc\nworktree=/private/rust\ndot=/private/rust-dot\ndot_revision=222\nnonce=333.444\ngit_dev=3\ngit_ino=4\n",
    )
    .expect("Rust receipt");
    fs::write(
        shell.state.join("shdeps/shdeps.links"),
        format!("{}/.local/share/man/man1/shdeps.1\n", shell.home.display()),
    )
    .expect("shell link state");
    fs::write(
        rust.state.join("shdeps/shdeps.links"),
        format!("{}/.local/share/man/man1/shdeps.1\n", rust.home.display()),
    )
    .expect("Rust link state");
    fs::write(
        shell.state.join("shdeps/shdeps.self-update.stamp"),
        b"1700000000\n",
    )
    .expect("shell update stamp");
    // The provider state lock embeds the holder pid and acquisition time on
    // both sides; it is runtime metadata, not converged content.
    fs::write(
        shell.state.join("shdeps/.lock"),
        format!(
            "pid=111\nstate_dir={}/shdeps\nacquired_unix=1700000000\n",
            shell.state.display()
        ),
    )
    .expect("shell provider lock");
    fs::write(
        rust.state.join("shdeps/.lock"),
        format!(
            "pid=222\nstate_dir={}/shdeps\nacquired_unix=1700000999\n",
            rust.state.display()
        ),
    )
    .expect("rust provider lock");

    assert_eq!(snapshot_client(&shell), snapshot_client(&rust));
    fs::write(
        rust.state.join("dot/init/completed"),
        b"phase=incomplete\norigin=fixture\nbranch=main\ncommit=abc\nworktree=/private/rust\ndot=/private/rust-dot\ndot_revision=222\nnonce=333.444\ngit_dev=3\ngit_ino=4\n",
    )
    .expect("mutated Rust receipt");
    assert_ne!(snapshot_client(&shell), snapshot_client(&rust));

    fs::write(
        rust.state.join("dot/init/completed"),
        b"phase=complete\norigin=fixture\nbranch=main\ncommit=abc\nworktree=/private/rust\ndot=/private/rust-dot\ndot_revision=222\nnonce=333.444\ngit_dev=3\ngit_ino=4\n",
    )
    .expect("restore Rust receipt");
    assert_eq!(snapshot_client(&shell), snapshot_client(&rust));
    fs::write(
        rust.state.join("shdeps/significant"),
        b"persistent provider state\n",
    )
    .expect("significant provider state");
    assert_ne!(snapshot_client(&shell), snapshot_client(&rust));
}

#[test]
fn state_snapshot_ignores_directories_holding_only_excluded_markers() {
    let scratch = Scratch::new("perf-excluded-only-dirs").expect("scratch");
    let shell = empty_client(&scratch, "shell");
    let rust = empty_client(&scratch, "rust");
    // The shell trampoline records its interpreter hint under `dot/` on every
    // invocation (including `help`); the native engine has no resolver and
    // writes nothing. Parity must ignore a directory whose entire on-disk
    // content is already excluded.
    fs::create_dir_all(shell.state.join("dot")).expect("shell state directory");
    fs::write(shell.state.join("dot/bash-v1"), b"shell runtime marker\n").expect("shell marker");
    fs::create_dir_all(shell.state.join("shdeps")).expect("shell provider directory");
    fs::write(
        shell.state.join("shdeps/shdeps.self-update.stamp"),
        b"1700000000\n",
    )
    .expect("shell update stamp");
    assert_eq!(
        snapshot_client(&shell),
        snapshot_client(&rust),
        "directories holding only excluded markers must not break parity"
    );

    // Non-excluded content under the same directories is still converged.
    fs::write(shell.state.join("dot/unexpected"), b"significant\n").expect("significant state");
    assert_ne!(
        snapshot_client(&shell),
        snapshot_client(&rust),
        "non-excluded state under dot/ must break parity"
    );
    fs::remove_file(shell.state.join("dot/unexpected")).expect("remove significant state");

    // Directory entries are structural, so an asymmetric empty directory is
    // also ignored: excluded-only content compares equal to missing or empty.
    fs::create_dir_all(rust.state.join("dot")).expect("rust state directory");
    assert_eq!(
        snapshot_client(&shell),
        snapshot_client(&rust),
        "an asymmetric empty directory must not break parity"
    );
}

#[test]
fn performance_artifacts_record_validated_pair_order_and_nanoseconds() {
    let scratch = Scratch::new("perf-artifacts").expect("scratch");
    let run_id = "9".repeat(40);
    let mut artifacts = Artifacts::new(scratch.path(), &run_id);
    artifacts.record_pair(
        Workload::DisjointClean,
        0,
        [EngineKind::Shell, EngineKind::Rust],
        11,
        7,
        0,
    );
    drop(artifacts);

    assert_eq!(
        fs::read(scratch.path().join("samples.tsv")).expect("samples"),
        b"run_id\tengine\tworkload\titeration\torder\tposition\telapsed_ns\texit_code\tvalidated\n\
9999999999999999999999999999999999999999\tshell\tdisjoint-clean\t1\tshell-rust\t1\t11\t0\ttrue\n\
9999999999999999999999999999999999999999\trust\tdisjoint-clean\t1\tshell-rust\t2\t7\t0\ttrue\n"
    );
}

#[test]
fn invalid_first_spawn_cannot_be_recorded() {
    let scratch = Scratch::new("perf-first-spawn-artifact").expect("scratch");
    let run_id = "9".repeat(40);
    let mut artifacts = Artifacts::new(scratch.path(), &run_id);
    assert!(
        artifacts
            .record_first_spawn(17, Err("invalid first-spawn observation".to_string()))
            .is_err()
    );
    drop(artifacts);

    assert_eq!(
        fs::read(scratch.path().join("samples.tsv")).expect("samples"),
        b"run_id\tengine\tworkload\titeration\torder\tposition\telapsed_ns\texit_code\tvalidated\n"
    );
}

#[test]
fn first_spawn_validation_requires_exact_output_and_unchanged_state() {
    let scratch = Scratch::new("perf-first-spawn-state").expect("scratch");
    let client = empty_client(&scratch, "client");
    let before = snapshot_complete_client(&client);
    let good = Command::new("/bin/sh")
        .args(["-c", "printf 'usage\\n'"])
        .output()
        .expect("run output fixture");
    assert!(validate_first_spawn(&good, b"usage\n", &before, &before).is_ok());
    assert!(validate_first_spawn(&good, b"different\n", &before, &before).is_err());
    fs::write(client.tmp.join("unexpected"), b"temporary residue")
        .expect("write temporary side effect");
    let changed = snapshot_complete_client(&client);
    assert!(validate_first_spawn(&good, b"usage\n", &before, &changed).is_err());
    fs::remove_file(client.tmp.join("unexpected")).expect("remove temporary side effect");
    fs::create_dir_all(client.state.join(".git")).expect("side-effect directory");
    fs::write(client.state.join(".git/unexpected"), b"state").expect("write side effect");
    let changed = snapshot_complete_client(&client);
    assert!(validate_first_spawn(&good, b"usage\n", &before, &changed).is_err());

    let stderr = Command::new("/bin/sh")
        .args(["-c", "printf warning >&2"])
        .output()
        .expect("run stderr fixture");
    assert!(validate_first_spawn(&stderr, b"", &before, &before).is_err());

    let failure = Command::new("/bin/sh")
        .args(["-c", "exit 7"])
        .output()
        .expect("run failure fixture");
    assert!(validate_first_spawn(&failure, b"", &before, &before).is_err());
}

#[test]
fn payload_validation_rejects_a_missing_base_file() {
    let scratch = Scratch::new("perf-base-payload").expect("scratch");
    let home = scratch.path().join("home");
    fs::create_dir_all(&home).expect("home");
    for overlay in 0..OVERLAYS {
        for index in 0..FILES_PER_OVERLAY {
            fs::write(
                home.join(format!("overlay-{overlay}-file-{index:03}.txt")),
                format!("overlay-{overlay} payload {index}\n"),
            )
            .expect("overlay payload");
        }
    }

    let error = validate_payloads(&home, None).expect_err("base payload is mandatory");
    assert!(error.contains(".testrc"));
}

#[test]
fn filesystem_metadata_targets_the_fixture_not_only_the_source_tree() {
    let source = Path::new("/source");
    let binary = Path::new("/build/release/dot");
    let fixture = Path::new("/fixture");
    let targets = filesystem_probe_targets(source, binary, fixture);

    assert_eq!(targets[0], ("fixture_filesystem", fixture.to_path_buf()));
    assert!(targets.contains(&("source_filesystem", source.to_path_buf())));
    assert!(targets.contains(&(
        "binary_filesystem",
        binary.parent().expect("binary parent").to_path_buf()
    )));
}

#[test]
fn filesystem_metadata_omits_device_mount_and_private_option_values() {
    let mountinfo =
        b"36 25 0:31 / / rw,relatime - overlay PRIVATE_DEVICE rw,lowerdir=/PRIVATE_WORKSPACE\n\
37 36 0:32 / /PRIVATE_MOUNT rw,nosuid,nodev,noexec - tmpfs PRIVATE_DEVICE rw,size=1024k\n";
    let description = filesystem_description_from_mountinfo(
        Path::new("/PRIVATE_MOUNT/PRIVATE_SCRATCH"),
        mountinfo,
    )
    .expect("matching filesystem");

    assert_eq!(description, "type=tmpfs;options=nodev,noexec,nosuid,rw");
    for private in [
        "PRIVATE_DEVICE",
        "PRIVATE_MOUNT",
        "PRIVATE_SCRATCH",
        "PRIVATE_WORKSPACE",
        "lowerdir",
    ] {
        assert!(!description.contains(private), "leaked {private}");
    }
}

#[test]
fn filesystem_metadata_maps_unknown_types_to_a_public_label() {
    let mountinfo = b"36 25 0:31 / / rw,relatime - PRIVATE_FS_TYPE PRIVATE_DEVICE rw\n";
    let description = filesystem_description_from_mountinfo(Path::new("/fixture"), mountinfo)
        .expect("matching filesystem");

    assert_eq!(description, "type=other;options=relatime,rw");
    assert!(!description.contains("PRIVATE_FS_TYPE"));
}

#[test]
fn hardware_metadata_uses_only_public_buckets() {
    assert_eq!(public_cpu_class(1), "single");
    assert_eq!(public_cpu_class(4), "small");
    assert_eq!(public_cpu_class(12), "medium");
    assert_eq!(public_cpu_class(64), "large");
    assert_eq!(public_runner_image(None), "local");
    assert_eq!(public_runner_image(Some(OsStr::new(""))), "local");
    assert_eq!(
        public_runner_image(Some(OsStr::new("ubuntu24"))),
        "ubuntu24"
    );
    assert_eq!(
        public_runner_image(Some(OsStr::new("PRIVATE_RUNNER_SENTINEL"))),
        "other"
    );
    assert_eq!(
        public_runner_image(Some(OsStr::from_bytes(b"ubuntu24\xffprivate"))),
        "other"
    );
    for label in ["local", "other", "ubuntu20", "ubuntu22", "ubuntu24"] {
        assert_eq!(validate_public_runner_label(label), Some(label));
    }
    assert_eq!(validate_public_runner_label(""), None);
    assert_eq!(validate_public_runner_label("private-runner"), None);
}

#[test]
fn tool_versions_are_parsed_into_a_public_grammar() {
    assert_eq!(
        parse_public_tool_version("git", b"git version 2.52.0\n"),
        Some("git 2.52.0".to_string())
    );
    assert_eq!(
        parse_public_tool_version(
            "bash",
            b"GNU bash, version 5.1.8(1)-release (x86_64-example-linux)\nCopyright ignored\n"
        ),
        Some("bash 5.1.8(1)-release".to_string())
    );
    assert_eq!(
        parse_public_tool_version("cargo", b"cargo 1.97.1 (012345678 2026-01-01)\n"),
        Some("cargo 1.97.1".to_string())
    );
    assert_eq!(
        parse_public_tool_version("rustc", b"rustc 1.97.1 (012345678 2026-01-01)\n"),
        Some("rustc 1.97.1".to_string())
    );
    assert_eq!(
        parse_public_tool_version("cargo", b"cargo /PRIVATE_TOOL_PATH\n"),
        None
    );
    assert_eq!(
        parse_public_tool_version("rustc", b"rustc 1.97.1\x01private\n"),
        None
    );
    assert_eq!(
        parse_public_tool_version("cargo", b"cargo PRIVATE_LABEL\n"),
        None
    );
    assert_eq!(
        parse_public_tool_version("git", b"git version INTERNAL_BUILD\n"),
        None
    );
    assert_eq!(
        parse_public_tool_version("rustc", b"rustc 1.85.1-private_project\n"),
        None
    );
    assert_eq!(
        parse_public_tool_version("rustc", b"rustc 1.85.1-nightly\n"),
        Some("rustc 1.85.1-nightly".to_string())
    );
    assert_eq!(
        parse_public_tool_version("cargo", b"cargo 1.85.1-beta.2\n"),
        Some("cargo 1.85.1-beta.2".to_string())
    );
    assert_eq!(parse_public_tool_version("unknown", b"unknown 1.0\n"), None);
}

#[test]
#[cfg(target_os = "linux")]
fn metadata_records_reproducible_identities_without_private_paths() {
    let scratch = Scratch::new("perf-metadata").expect("scratch");
    let tools = PerfTools::system().expect("system performance tools");
    let provider = scratch.path().join("PRIVATE_HOME_SENTINEL-provider");
    let shell_executable = scratch.path().join("PRIVATE_DEVICE_SENTINEL-shell/bin/dot");
    let rust_executable = scratch
        .path()
        .join("PRIVATE_MOUNT_SENTINEL-build/release/dot");
    fs::create_dir_all(shell_executable.parent().expect("shell binary parent"))
        .expect("shell binary directory");
    fs::create_dir_all(rust_executable.parent().expect("Rust binary parent"))
        .expect("Rust binary directory");
    fs::copy(&tools.bash, &shell_executable).expect("shell binary fixture");
    fs::copy(&tools.git, &rust_executable).expect("Rust binary fixture");
    fs::create_dir_all(provider.join("target/release")).expect("provider binary directory");
    fs::copy(&tools.git, provider.join("target/release/shdeps")).expect("provider binary");
    git(&tools, &provider, &["init", "-q"]);
    git(&tools, &provider, &["config", "user.name", "fixture"]);
    git(
        &tools,
        &provider,
        &["config", "user.email", "fixture@example.invalid"],
    );
    fs::write(provider.join("tracked"), b"tracked\n").expect("provider tracked file");
    git(&tools, &provider, &["add", "tracked"]);
    git(&tools, &provider, &["commit", "-qm", "fixture"]);
    let engines = Engines {
        shell: Engine {
            kind: EngineKind::Shell,
            executable: shell_executable,
            source_root: scratch.path().join("PRIVATE_WORKSPACE_SENTINEL-shell"),
            commit: "a".repeat(40),
        },
        rust: Engine {
            kind: EngineKind::Rust,
            executable: rust_executable,
            source_root: scratch.path().join("PRIVATE_WORKSPACE_SENTINEL-rust"),
            commit: "b".repeat(40),
        },
    };
    let artifact_dir = scratch.path().join("PRIVATE_SCRATCH_SENTINEL-artifacts");
    let run_id = "9".repeat(40);
    let artifacts = Artifacts::new(&artifact_dir, &run_id);
    let provider_identity = ProviderIdentity {
        current_revision: git_text(&tools, &provider, &["rev-parse", "HEAD"]),
        shell_revision: "1".repeat(40),
        abi: "1".to_string(),
    };
    let evidence = EvidenceIdentity {
        run_id: run_id.clone(),
        shell_commit: engines.shell.commit.clone(),
        shell_tree: "c".repeat(40),
        rust_commit: engines.rust.commit.clone(),
        rust_tree: "d".repeat(40),
        shdeps_commit: provider_identity.current_revision.clone(),
        shdeps_tree: "e".repeat(40),
        git_blob: file_blob(&tools, &tools.git),
        bash_blob: file_blob(&tools, &tools.bash),
        cargo_blob: file_blob(&tools, &tools.cargo),
        rustc_blob: file_blob(&tools, &tools.rustc),
        shell_executable_blob: file_blob(&tools, &engines.shell.executable),
        rust_executable_blob: file_blob(&tools, &engines.rust.executable),
        shdeps_executable_blob: file_blob(&tools, &provider.join("target/release/shdeps")),
    };
    artifacts.write_metadata(
        &engines,
        &tools,
        (&provider, &provider_identity),
        &evidence,
        MetadataContext {
            fixture_root: &scratch.path().join("PRIVATE_MOUNT_SENTINEL-fixture"),
            dirty: "false",
            runner_image: "local",
        },
    );
    let metadata = fs::read_to_string(artifact_dir.join("metadata.tsv")).expect("metadata");
    assert_eq!(metadata.lines().count(), 51, "exact metadata schema");

    for expected in [
        "format\tperformance-metadata-v2".to_string(),
        format!("run_id\t{run_id}"),
        "git_location\tsystem-tool".to_string(),
        "bash_location\tsystem-tool".to_string(),
        "cargo_location\tselected-build-tool".to_string(),
        "rustc_location\tselected-build-tool".to_string(),
        "client_path_policy\tcontrolled".to_string(),
        "artifact_location\tconfigured-output".to_string(),
        "shell_executable_location\thistorical-checkout/bin/dot".to_string(),
        "rust_executable_location\trun-private-target/dot/release/dot".to_string(),
        "shdeps_executable_location\trun-private-target/shdeps/release/shdeps".to_string(),
        format!(
            "current_shdeps_lock_revision\t{}",
            provider_identity.current_revision
        ),
        format!(
            "shell_shdeps_lock_revision\t{}",
            provider_identity.shell_revision
        ),
    ] {
        assert!(metadata.lines().any(|line| line == expected), "{expected}");
    }
    for key in [
        "git_version",
        "git_blob",
        "bash_version",
        "bash_blob",
        "cargo_version",
        "cargo_blob",
        "rustc_version",
        "rustc_blob",
    ] {
        assert!(
            metadata
                .lines()
                .any(|line| line.starts_with(&format!("{key}\t"))),
            "missing {key}"
        );
    }
    for key in [
        "fixture_filesystem",
        "source_filesystem",
        "binary_filesystem",
    ] {
        let row = metadata
            .lines()
            .find(|line| line.starts_with(&format!("{key}\t")))
            .unwrap_or_else(|| panic!("missing {key}"));
        assert!(row.contains("type="));
        assert!(row.contains(";options="));
    }
    let cpu_count = metadata
        .lines()
        .find_map(|line| line.strip_prefix("cpu_count\t"))
        .expect("CPU count")
        .parse::<usize>()
        .expect("numeric CPU count");
    assert!(cpu_count > 0);
    assert!(
        metadata
            .lines()
            .any(|line| { line == format!("cpu_class\t{}", public_cpu_class(cpu_count)) })
    );
    assert!(metadata.lines().any(|line| {
        matches!(
            line.strip_prefix("runner_image\t"),
            Some("local" | "other" | "ubuntu20" | "ubuntu22" | "ubuntu24")
        )
    }));
    for private in [
        "PRIVATE_HOME_SENTINEL",
        "PRIVATE_WORKSPACE_SENTINEL",
        "PRIVATE_SCRATCH_SENTINEL",
        "PRIVATE_DEVICE_SENTINEL",
        "PRIVATE_MOUNT_SENTINEL",
        &artifact_dir.to_string_lossy(),
        &provider.to_string_lossy(),
        &tools.client_path.to_string_lossy(),
    ] {
        assert!(!metadata.contains(private), "metadata leaked {private}");
    }
    for line in metadata.lines().skip(1) {
        let (_, value) = line.split_once('\t').expect("metadata key/value row");
        assert!(!value.starts_with('/'), "metadata leaked an absolute path");
    }
}

#[test]
fn failure_state_parity_covers_home_all_xdg_roots_and_tmp() {
    let scratch = Scratch::new("failure-state-parity").expect("scratch");
    let clients = Clients {
        shell: empty_client(&scratch, "shell"),
        rust: empty_client(&scratch, "rust"),
    };
    let steady_shell = snapshot_client(&clients.shell);
    let steady_rust = snapshot_client(&clients.rust);
    validate_paired_client_state(&clients, &steady_shell, &steady_rust)
        .expect("empty clients have parity");

    for root in ClientRoot::ALL {
        let marker = root.path(&clients.rust).join("engine-only-mutation");
        fs::create_dir_all(marker.parent().expect("mutation parent"))
            .expect("create mutation parent");
        fs::write(&marker, b"changed\n").expect("write engine-only mutation");
        let error = validate_paired_client_state(&clients, &steady_shell, &steady_rust)
            .expect_err("every persistent root mutation must fail parity");
        assert!(error.contains(root.label()), "{error}");
        fs::remove_file(marker).expect("remove mutation");
    }
}

#[test]
#[cfg(target_os = "linux")]
fn timed_command_bounds_and_reaps_a_hanging_descendant() {
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::Duration;

    let scratch = Scratch::new_exec("perf-timeout").expect("scratch");
    let script = scratch.path().join("hang.sh");
    let child_pid = scratch.path().join("child.pid");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\ntrap '' TERM\nset -m\n( trap '' TERM; sleep 30 ) &\nprintf '%s\\n' \"$!\" >'{}'\nwait\n",
            child_pid.display()
        ),
    )
    .expect("write hanging command");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).expect("chmod hanging command");
    let mut command = Command::new(&script);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let started = Instant::now();
    let error = run_timed_command(&mut command, Duration::from_millis(250))
        .expect_err("hanging command must time out");
    assert!(error.contains("timed out"));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "timeout cleanup exceeded its bounded deadline"
    );
    let pid = fs::read_to_string(&child_pid)
        .expect("descendant pid")
        .trim()
        .parse::<u32>()
        .expect("numeric descendant pid");
    assert!(!process_is_live(pid), "timed-out descendant {pid} survived");
}

#[test]
#[cfg(target_os = "linux")]
fn timed_command_accepts_only_a_quiescent_success() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "printf 'complete\\n'"]);

    let output = run_timed_command(&mut command, Duration::from_secs(2))
        .expect("quiescent command succeeds");
    assert!(output.output.status.success());
    assert_eq!(output.output.stdout, b"complete\n");
    assert!(output.output.stderr.is_empty());
}

#[test]
#[cfg(target_os = "linux")]
fn timed_command_allows_a_short_lived_descendant_to_quiesce() {
    let scratch = Scratch::new_exec("perf-quiescence").expect("scratch");
    let marker = scratch.path().join("required-marker");
    let script = format!(
        "( sleep 0.15; printf complete >'{}' ) </dev/null >/dev/null 2>&1 & exit 0",
        marker.display()
    );
    let mut command = Command::new("/bin/sh");
    command.args(["-c", &script]);

    let output = run_timed_command(&mut command, Duration::from_secs(2))
        .expect("a child that quiesces within the bounded grace period succeeds");
    assert!(output.output.status.success());
    assert_eq!(
        fs::read(marker).expect("delayed required marker"),
        b"complete"
    );
    assert!(
        output.elapsed_ns >= Duration::from_millis(100).as_nanos(),
        "elapsed time excluded required descendant work: {}ns",
        output.elapsed_ns
    );
}

#[test]
#[cfg(target_os = "linux")]
fn timed_command_rejects_quiescence_after_the_total_deadline() {
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        "( sleep 0.45 ) </dev/null >/dev/null 2>&1 & sleep 0.25; exit 0",
    ]);

    let started = Instant::now();
    let error = run_timed_command(&mut command, Duration::from_millis(350))
        .expect_err("descendant quiescence after the command deadline must fail");
    assert!(
        error.contains("live descendant") || error.contains("timed out"),
        "{error}"
    );
    assert!(
        started.elapsed() < Duration::from_millis(800),
        "total deadline was extended by the quiescence grace"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn timed_command_rejects_oversized_stdout_capture() {
    let head = system_tool("head").expect("Linux head tool");
    let mut command = Command::new("/bin/sh");
    command.args(["-c", &format!("'{}' -c 8388608 /dev/zero", head.display())]);

    let started = Instant::now();
    let error = run_timed_command_with_capture_limit(&mut command, Duration::from_secs(2), 128)
        .expect_err("oversized stdout must fail explicitly");
    assert!(error.contains("stdout exceeded 128 bytes"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
#[cfg(target_os = "linux")]
fn timed_command_rejects_oversized_stderr_capture() {
    let head = system_tool("head").expect("Linux head tool");
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        &format!("'{}' -c 8388608 /dev/zero >&2", head.display()),
    ]);

    let started = Instant::now();
    let error = run_timed_command_with_capture_limit(&mut command, Duration::from_secs(2), 128)
        .expect_err("oversized stderr must fail explicitly");
    assert!(error.contains("stderr exceeded 128 bytes"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
#[cfg(target_os = "linux")]
fn bounded_capture_discards_excess_bytes_without_growing_storage() {
    let input = std::io::Cursor::new(vec![b'x'; 8 * 1024 * 1024]);
    let capture = drain_bounded_capture(input, 128).expect("drain bounded capture");

    assert!(capture.exceeded);
    assert_eq!(capture.bytes.len(), 128);
}

#[test]
#[cfg(target_os = "linux")]
fn linux_process_identity_parser_is_strict_and_records_start_time() {
    let fields = [
        "S", "1", "2", "44", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "0", "1",
        "0", "777",
    ];
    let stat = format!("42 (fixture name) {}\n", fields.join(" "));
    let identity = parse_linux_process_identity(42, stat.as_bytes()).expect("valid process record");
    assert_eq!(identity.parent, 1);
    assert_eq!(identity.session, 44);
    assert_eq!(identity.start, 777);
    assert!(identity.live);
    for state in ["Z", "X", "x"] {
        let mut terminal = fields;
        terminal[0] = state;
        let stat = format!("42 (fixture name) {}\n", terminal.join(" "));
        let identity =
            parse_linux_process_identity(42, stat.as_bytes()).expect("terminal process record");
        assert!(!identity.live, "state {state} must be terminal");
    }
    assert!(parse_linux_process_identity(42, b"partial").is_err());
    assert!(process_vanished(&std::io::Error::from_raw_os_error(
        libc::ENOENT
    )));
    assert!(process_vanished(&std::io::Error::from_raw_os_error(
        libc::ESRCH
    )));
    assert!(!process_vanished(&std::io::Error::from_raw_os_error(
        libc::EACCES
    )));
}

#[test]
#[cfg(target_os = "linux")]
fn supervisor_record_reader_rejects_a_truncated_length() {
    let scratch = Scratch::new("perf-records").expect("scratch");
    let records = scratch.path().join("records");
    fs::write(&records, [0_u8; 3]).expect("write truncated record length");

    let error = read_records(&records).expect_err("a partial record length must fail closed");
    assert!(error.contains("length"), "unexpected error: {error}");
}

#[test]
#[cfg(target_os = "linux")]
fn supervisor_record_reader_rejects_an_oversized_length_prefix() {
    let scratch = Scratch::new("perf-record-prefix").expect("scratch");
    let records = scratch.path().join("records");
    fs::write(&records, (64_u64 * 1024 + 1).to_le_bytes()).expect("write oversized record length");

    let error =
        read_records(&records).expect_err("an oversized record must fail before allocation");
    assert!(error.contains("too large"), "unexpected error: {error}");
}

#[test]
#[cfg(target_os = "linux")]
fn supervisor_record_reader_rejects_an_oversized_payload_file() {
    let scratch = Scratch::new("perf-record-payload").expect("scratch");
    let records = scratch.path().join("records");
    let mut encoded = Vec::new();
    for _ in 0..9 {
        encoded.extend_from_slice(&(64_u64 * 1024).to_le_bytes());
        encoded.extend(std::iter::repeat_n(b'x', 64 * 1024));
    }
    fs::write(&records, encoded).expect("write oversized aggregate payload");

    let error = read_records(&records).expect_err("an oversized payload file must fail closed");
    assert!(error.contains("total"), "unexpected error: {error}");
}

#[test]
#[cfg(target_os = "linux")]
fn timed_command_reaps_a_descendant_left_by_a_successful_parent() {
    let scratch = Scratch::new_exec("perf-success-descendant").expect("scratch");
    let script = scratch.path().join("leak.sh");
    let child_pid = scratch.path().join("child.pid");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nset -m\n( trap '' TERM; sleep 30 ) &\nprintf '%s\\n' \"$!\" >'{}'\nexit 0\n",
            child_pid.display()
        ),
    )
    .expect("write descendant fixture");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
        .expect("chmod descendant fixture");
    let mut command = Command::new(&script);
    let started = Instant::now();
    let error = run_timed_command(&mut command, Duration::from_secs(2))
        .expect_err("a successful leader with live descendants is invalid");
    assert!(error.contains("live descendant"));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "successful-parent cleanup exceeded its bounded deadline"
    );
    let pid = fs::read_to_string(&child_pid)
        .expect("descendant pid")
        .trim()
        .parse::<u32>()
        .expect("numeric descendant pid");
    assert!(
        !process_is_live(pid),
        "successful descendant {pid} survived"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn timed_command_rejects_and_reaps_an_escaped_setsid_descendant() {
    let scratch = Scratch::new_exec("perf-escaped-descendant").expect("scratch");
    let script = scratch.path().join("escape.sh");
    let child_pid = scratch.path().join("child.pid");
    let setsid = system_tool("setsid").expect("Linux setsid tool");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n'{}' /bin/sh -c 'trap \"\" TERM; printf \"%s\\n\" \"$$\" >\"$1\"; sleep 30' _ '{}' </dev/null >/dev/null 2>&1 &\nwhile [ ! -s '{}' ]; do :; done\nexit 0\n",
            setsid.display(),
            child_pid.display(),
            child_pid.display(),
        ),
    )
    .expect("write escaped-descendant fixture");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
        .expect("chmod escaped-descendant fixture");
    let mut command = Command::new(&script);

    let result = run_timed_command(&mut command, Duration::from_secs(2));
    let pid = fs::read_to_string(&child_pid)
        .expect("escaped descendant pid")
        .trim()
        .parse::<u32>()
        .expect("numeric escaped descendant pid");
    if result.is_ok() {
        // Keep the RED regression bounded when the old session-only supervisor
        // misses a child that deliberately created a new session.
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
    let error = result.expect_err("escaped live descendant must invalidate the command");
    assert!(error.contains("live descendant"), "{error}");
    assert!(!process_is_live(pid), "escaped descendant {pid} survived");
}

#[test]
#[cfg(target_os = "linux")]
fn direct_child_authority_precedes_a_failing_broad_process_scan() {
    let scratch = Scratch::new_exec("perf-direct-before-broad").expect("scratch");
    let script = scratch.path().join("escape.sh");
    let child_pid = scratch.path().join("child.pid");
    let lock_path = scratch.path().join("inherited-lock");
    let setsid = system_tool("setsid").expect("Linux setsid tool");
    let flock = system_tool("flock").expect("Linux flock tool");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nexec 9>'{}'\n'{}' --exclusive --nonblock 9 || exit 91\n'{}' /bin/sh -c 'trap \"\" TERM; printf \"%s\\n\" \"$$\" >\"$1\"; kill -STOP $$; while :; do :; done' _ '{}' &\nwhile [ ! -s '{}' ]; do :; done\nexit 0\n",
            lock_path.display(),
            flock.display(),
            setsid.display(),
            child_pid.display(),
            child_pid.display(),
        ),
    )
    .expect("write direct-before-broad fixture");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
        .expect("chmod direct-before-broad fixture");
    let started = Instant::now();
    let worker_script = script.clone();
    let worker_marker = child_pid.clone();
    let (sender, receiver) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let mut command = Command::new(worker_script);
        let result = run_timed_command_with_scan_fault_marker(
            &mut command,
            Duration::from_secs(2),
            CAPTURE_LIMIT_BYTES,
            Some("direct-before-fail"),
            Some(&worker_marker),
        );
        let _ = sender.send(result);
    });
    let pid = wait_for_process_marker(&child_pid, Duration::from_secs(2))
        .expect("escaped descendant marker");
    let escaped_guard =
        ExactProcessGuard::acquire(pid).expect("bind exact escaped fixture identity");
    let result = match receiver.recv_timeout(Duration::from_secs(5)) {
        Ok(result) => result,
        Err(error) => {
            drop(escaped_guard);
            let _ = worker.join();
            panic!("direct-first supervisor exceeded its watchdog: {error}");
        }
    };
    worker.join().expect("join direct-first supervisor");
    let error = result.expect_err("broad process-scan failure must reject the sample");
    assert!(error.contains("process-table failure"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "direct-first cleanup exceeded its bounded deadline"
    );
    assert!(
        !escaped_guard.is_live(),
        "directly pinned escaped child {pid} survived broad-scan failure"
    );
    assert!(
        Command::new(&flock)
            .args(["--exclusive", "--nonblock"])
            .arg(&lock_path)
            .arg("/bin/true")
            .status()
            .expect("probe released fixture lock")
            .success(),
        "directly pinned escaped child retained its inherited lock"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn retained_pidfd_survives_an_omitted_then_failed_process_snapshot() {
    let scratch = Scratch::new_exec("perf-retained-process-identity").expect("scratch");
    let script = scratch.path().join("escape.sh");
    let child_pid = scratch.path().join("child.pid");
    let lock_path = scratch.path().join("inherited-lock");
    let setsid = system_tool("setsid").expect("Linux setsid tool");
    let flock = system_tool("flock").expect("Linux flock tool");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nexec 9>'{}'\n'{}' --exclusive --nonblock 9 || exit 91\n'{}' /bin/sh -c 'trap \"\" TERM; printf \"%s\\n\" \"$$\" >\"$1\"; kill -STOP $$; while :; do :; done' _ '{}' &\nwhile [ ! -s '{}' ]; do :; done\nexit 0\n",
            lock_path.display(),
            flock.display(),
            setsid.display(),
            child_pid.display(),
            child_pid.display(),
        ),
    )
    .expect("write omitted-snapshot fixture");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
        .expect("chmod omitted-snapshot fixture");
    let before = fs::read_dir("/proc/self/fd")
        .expect("initial descriptor inventory")
        .count();
    let started = Instant::now();
    let worker_script = script.clone();
    let (sender, receiver) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let mut command = Command::new(worker_script);
        let result = run_timed_command_with_scan_fault(
            &mut command,
            Duration::from_secs(2),
            CAPTURE_LIMIT_BYTES,
            Some("omit-then-fail"),
        );
        let _ = sender.send(result);
    });
    let pid = wait_for_process_marker(&child_pid, Duration::from_secs(2))
        .expect("escaped descendant marker");
    let escaped_guard =
        ExactProcessGuard::acquire(pid).expect("bind exact escaped fixture identity");
    let result = match receiver.recv_timeout(Duration::from_secs(5)) {
        Ok(result) => result,
        Err(error) => {
            drop(escaped_guard);
            let _ = worker.join();
            panic!("retained-identity cleanup exceeded its watchdog: {error}");
        }
    };
    worker.join().expect("join retained-identity supervisor");
    let lock_available = Command::new(&flock)
        .args(["--exclusive", "--nonblock"])
        .arg(&lock_path)
        .arg("/bin/true")
        .status()
        .expect("probe inherited fixture lock")
        .success();
    let error = result.expect_err("omitted then failed scan must fail closed");
    assert!(error.contains("process-table failure"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "omitted-snapshot cleanup or capture join was not bounded"
    );
    assert!(
        !escaped_guard.is_live(),
        "retained escaped child {pid} survived"
    );
    assert!(lock_available, "escaped child retained its inherited lock");
    let after = fs::read_dir("/proc/self/fd")
        .expect("final descriptor inventory")
        .count();
    assert!(
        after <= before + 1,
        "process identity handles leaked after cleanup: {before} -> {after}"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn total_observation_loss_rejects_without_blocking_capture_readers() {
    let scratch = Scratch::new_exec("perf-unobserved-process").expect("scratch");
    let script = scratch.path().join("escape.sh");
    let child_pid = scratch.path().join("child.pid");
    let lock_path = scratch.path().join("inherited-lock");
    let setsid = system_tool("setsid").expect("Linux setsid tool");
    let flock = system_tool("flock").expect("Linux flock tool");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nexec 9>'{}'\n'{}' --exclusive --nonblock 9 || exit 91\n'{}' /bin/sh -c 'trap \"\" TERM; printf \"%s\\n\" \"$$\" >\"$1\"; kill -STOP $$; while :; do :; done' _ '{}' &\nwhile [ ! -s '{}' ]; do :; done\nexit 0\n",
            lock_path.display(),
            flock.display(),
            setsid.display(),
            child_pid.display(),
            child_pid.display(),
        ),
    )
    .expect("write total-observation-loss fixture");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
        .expect("chmod total-observation-loss fixture");
    let before = fs::read_dir("/proc/self/fd")
        .expect("initial descriptor inventory")
        .count();
    let started = Instant::now();
    let worker_script = script.clone();
    let (sender, receiver) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let mut command = Command::new(worker_script);
        let result = run_timed_command_with_scan_fault(
            &mut command,
            Duration::from_secs(2),
            CAPTURE_LIMIT_BYTES,
            Some("unobserved-then-fail"),
        );
        let _ = sender.send(result);
    });
    let pid = wait_for_process_marker(&child_pid, Duration::from_secs(2))
        .expect("escaped descendant marker");
    let mut escaped_guard =
        ExactProcessGuard::acquire(pid).expect("bind exact escaped fixture identity");
    let result = match receiver.recv_timeout(Duration::from_secs(5)) {
        Ok(result) => result,
        Err(error) => {
            let cleanup = escaped_guard.terminate();
            let _ = worker.join();
            panic!("total observation loss exceeded its watchdog: {error}; cleanup={cleanup:?}");
        }
    };
    worker.join().expect("join observation-loss supervisor");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "total observation loss blocked on an inherited output pipe"
    );
    let error = result.expect_err("total observation loss must reject the sample");
    assert!(
        error.contains(&format!(
            "process-table failure after unretained omission of {pid}"
        )),
        "{error}"
    );
    assert!(
        error.contains("direct child process inventory is unavailable"),
        "{error}"
    );
    assert!(
        !Command::new(&flock)
            .args(["--exclusive", "--nonblock"])
            .arg(&lock_path)
            .arg("/bin/true")
            .status()
            .expect("probe retained fixture lock")
            .success(),
        "unobserved descendant did not retain lifecycle authority"
    );
    // This test alone knows the exact still-live identity. Production refuses
    // an unsafe numeric-PID fallback when observation is unavailable.
    escaped_guard
        .terminate()
        .expect("kill exact escaped fixture identity");
    assert!(!process_is_live(pid), "escaped fixture did not terminate");
    assert!(
        Command::new(&flock)
            .args(["--exclusive", "--nonblock"])
            .arg(&lock_path)
            .arg("/bin/true")
            .status()
            .expect("probe released fixture lock")
            .success(),
        "escaped fixture retained its lock after exact teardown"
    );
    let after = fs::read_dir("/proc/self/fd")
        .expect("final descriptor inventory")
        .count();
    assert!(
        after <= before + 1,
        "process identity or capture handles leaked: {before} -> {after}"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn timed_cleanup_does_not_signal_an_unrelated_session() {
    let mut control = Command::new("/bin/sh");
    control.args(["-c", "trap '' TERM; sleep 30"]);
    // SAFETY: the control child creates its own session before exec so the
    // benchmark cleanup has a concrete unrelated process to preserve.
    unsafe {
        control.pre_exec(|| {
            if libc::setsid() < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut control = control.spawn().expect("spawn unrelated control session");

    let scratch = Scratch::new_exec("perf-session-isolation").expect("scratch");
    let script = scratch.path().join("leak.sh");
    fs::write(
        &script,
        "#!/bin/sh\nset -m\n( trap '' TERM; sleep 30 ) &\nexit 0\n",
    )
    .expect("write descendant fixture");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
        .expect("chmod descendant fixture");
    let mut command = Command::new(&script);
    let error = run_timed_command(&mut command, Duration::from_secs(2))
        .expect_err("live descendant must invalidate the command");
    let control_survived = control.try_wait().expect("inspect control").is_none();

    signal_original_group(control.id(), libc::SIGKILL).expect("stop control session");
    let _ = control.wait();
    assert!(error.contains("live descendant"));
    assert!(control_survived, "cleanup signaled an unrelated session");
}

#[test]
fn rustup_style_build_tool_directory_is_a_supported_fallback() {
    let scratch = Scratch::new_exec("perf-rustup-tools").expect("scratch");
    let bin = scratch.path().join(".cargo/bin");
    fs::create_dir_all(&bin).expect("tool directory");
    let rustc = bin.join("rustc-fixture");
    fs::write(&rustc, b"#!/bin/sh\nexit 0\n").expect("tool fixture");
    fs::set_permissions(&rustc, fs::Permissions::from_mode(0o700)).expect("chmod tool fixture");
    let path = std::env::join_paths([bin]).expect("fixture PATH");

    assert_eq!(
        discover_build_tool("rustc-fixture", &path).expect("rustup-style tool"),
        fs::canonicalize(rustc).expect("canonical tool")
    );
}

#[test]
fn symlinked_proxy_tool_keeps_its_selected_name() {
    // Rustup proxies dispatch on argv[0]: resolving the `cargo` symlink to the
    // manager binary would report `manager --version` instead of `cargo --version`.
    // The manager also needs its resolution environment (like rustup needs HOME
    // to locate the toolchain), which the version probe must pass through.
    if std::env::var_os("HOME").is_none() {
        panic!("proxy tool test requires HOME in the test environment");
    }
    let scratch = Scratch::new_exec("perf-proxy-tools").expect("scratch");
    let bin = scratch.path().join("bin");
    fs::create_dir_all(&bin).expect("tool directory");
    let manager = bin.join("manager");
    fs::write(
        &manager,
        "#!/bin/sh\n[ -n \"$HOME\" ] || exit 1\ncase \"${0##*/}\" in\ncargo) printf 'cargo 1.2.3 (fixture)\\n';;\n*) printf 'manager 9.9.9 (fixture)\\n';;\nesac\n",
    )
    .expect("manager fixture");
    fs::set_permissions(&manager, fs::Permissions::from_mode(0o700)).expect("chmod manager");
    let proxy = bin.join("cargo");
    std::os::unix::fs::symlink(&manager, &proxy).expect("proxy symlink");
    let path = std::env::join_paths([bin]).expect("fixture PATH");

    let resolved = discover_build_tool("cargo", &path).expect("proxy tool");
    assert_eq!(
        resolved.file_name().expect("proxy file name"),
        OsStr::new("cargo")
    );
    assert_eq!(
        tool_version("cargo", &resolved, &["--version"]),
        "cargo 1.2.3"
    );
}

#[test]
fn shell_engine_is_spawned_by_the_exact_selected_bash() {
    let tools = PerfTools::system().expect("system performance tools");
    let engine = Engine {
        kind: EngineKind::Shell,
        executable: PathBuf::from("/fixture/bin/dot"),
        source_root: PathBuf::from("/fixture"),
        commit: "a".repeat(40),
    };
    let command = engine_command(&engine, &tools);

    assert_eq!(command.get_program(), tools.bash.as_os_str());
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        vec![engine.executable.as_os_str()]
    );
}

#[test]
fn provider_validation_allows_a_distinct_historical_lock() {
    let tools = PerfTools::system().expect("system performance tools");
    let scratch = Scratch::new("perf-provider-locks").expect("scratch");
    let current = scratch.path().join("current");
    let shell = scratch.path().join("shell");
    let provider = scratch.path().join("provider");
    for root in [&current, &shell] {
        fs::create_dir_all(root.join("support")).expect("lock directory");
    }
    fs::create_dir_all(provider.join("target/release/.fingerprint/stale"))
        .expect("provider directories");
    fs::write(provider.join("install.sh"), b"installer\n").expect("provider installer");
    fs::write(provider.join("shdeps.sh"), b"library\n").expect("provider library");
    fs::write(
        provider.join("target/release/shdeps"),
        b"stale checkout binary\n",
    )
    .expect("stale provider binary");
    fs::write(
        provider.join("target/release/.fingerprint/stale/output"),
        b"stale checkout fingerprint\n",
    )
    .expect("stale provider fingerprint");
    let provider_binary = scratch.path().join("run-private-target/release/shdeps");
    fs::create_dir_all(provider_binary.parent().expect("provider binary parent"))
        .expect("run-private provider target");
    fs::write(&provider_binary, b"current provider binary\n").expect("provider binary");
    fs::set_permissions(&provider_binary, fs::Permissions::from_mode(0o700))
        .expect("chmod provider binary");
    git(&tools, &provider, &["init", "-q"]);
    git(&tools, &provider, &["config", "user.name", "fixture"]);
    git(
        &tools,
        &provider,
        &["config", "user.email", "fixture@example.invalid"],
    );
    fs::write(provider.join("tracked"), b"tracked\n").expect("tracked provider file");
    git(&tools, &provider, &["add", "tracked"]);
    git(&tools, &provider, &["commit", "-qm", "fixture"]);
    let current_revision = git_text(&tools, &provider, &["rev-parse", "HEAD"]);
    let shell_revision = "1".repeat(40);
    fs::write(
        current.join("support/shdeps.lock"),
        format!("revision={current_revision}\nabi=1\n"),
    )
    .expect("current lock");
    fs::write(
        shell.join("support/shdeps.lock"),
        format!("revision={shell_revision}\nabi=1\n"),
    )
    .expect("historical lock");
    let engines = Engines {
        shell: Engine {
            kind: EngineKind::Shell,
            executable: tools.bash.clone(),
            source_root: shell,
            commit: "a".repeat(40),
        },
        rust: Engine {
            kind: EngineKind::Rust,
            executable: tools.git.clone(),
            source_root: current,
            commit: "b".repeat(40),
        },
    };

    let identity = validate_provider(&tools, &engines, &provider, &provider_binary)
        .expect("compatible provider");
    assert_eq!(identity.current_revision, current_revision);
    assert_eq!(identity.shell_revision, shell_revision);
    assert_eq!(identity.abi, "1");

    fs::write(
        engines.rust.source_root.join("support/shdeps.lock"),
        format!("revision={}\nabi=1\n", "2".repeat(40)),
    )
    .expect("mismatched current lock");
    assert!(
        validate_provider(&tools, &engines, &provider, &provider_binary)
            .expect_err("provider must match the current lock")
            .contains("expected current lock")
    );
}

#[test]
fn fixture_git_ignores_a_hostile_leading_path_entry() {
    let scratch = Scratch::new_exec("perf-hostile-path").expect("scratch");
    let hostile = scratch.path().join("hostile");
    fs::create_dir_all(&hostile).expect("hostile directory");
    let marker = scratch.path().join("wrapper-ran");
    let wrapper = hostile.join("git");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf called >'{}'\nexit 93\n",
            marker.display()
        ),
    )
    .expect("hostile wrapper");
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).expect("chmod wrapper");

    let tools = PerfTools::system().expect("system performance tools");
    let hostile_path = std::env::join_paths(
        std::iter::once(hostile.clone()).chain(std::env::split_paths(&tools.client_path)),
    )
    .expect("hostile path");
    assert_eq!(
        resolve_in_path(OsStr::new("git"), &hostile_path).expect("hostile git"),
        fs::canonicalize(&wrapper).expect("canonical wrapper")
    );

    let repo = scratch.path().join("repo");
    fs::create_dir_all(&repo).expect("repo");
    git(&tools, &repo, &["init", "-q"]);
    assert!(!marker.exists(), "fixture Git executed the leading wrapper");
    assert_eq!(
        resolve_in_path(OsStr::new("git"), &tools.client_path).expect("controlled git"),
        tools.git
    );
}

#[test]
fn performance_summary_requires_sample_count_ratio_and_p95() {
    let passing = Measurements {
        shell: vec![100; RUNS],
        rust: vec![75; RUNS],
    };
    assert!(summary_row(Workload::DisjointClean, &passing).passed);

    let slow_ratio = Measurements {
        shell: vec![100; RUNS],
        rust: vec![76; RUNS],
    };
    assert!(!summary_row(Workload::DisjointClean, &slow_ratio).passed);

    let slow_p95 = Measurements {
        shell: vec![10_000_000_000; RUNS],
        rust: vec![6_000_000_001; RUNS],
    };
    assert!(!summary_row(Workload::DisjointClean, &slow_p95).passed);

    let incomplete = Measurements {
        shell: vec![100; RUNS - 1],
        rust: vec![75; RUNS - 1],
    };
    assert!(!summary_row(Workload::DisjointClean, &incomplete).passed);

    let slow_composed = Measurements {
        shell: vec![100; RUNS],
        rust: vec![76; RUNS],
    };
    assert!(!summary_row(Workload::FeatureCollision, &slow_composed).passed);
}

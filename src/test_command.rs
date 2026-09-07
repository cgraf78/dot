//! Native suite authority, discovery, arguments and presentation. The public
//! reporter/timeout executables remain external suite interfaces; this command
//! never sources the shell test engine.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::app::{Runtime, Streams};
use crate::test_suites::{filter_matches, is_valid_suite_identity, resolve_jobs, suite_label};

pub(crate) struct Options {
    pub(crate) parallel: bool,
    pub(crate) verbose: bool,
    pub(crate) jobs: usize,
    pub(crate) requested_jobs: OsString,
    pub(crate) color: bool,
    pub(crate) child_style: bool,
    pub(crate) ci: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Provider,
    Local,
    Extension,
}

impl Source {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Local => "local",
            Self::Extension => "extension",
        }
    }
}

pub(crate) struct Suite {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) source: Source,
}

pub(crate) struct Context<'a> {
    pub(crate) runtime: &'a Runtime,
    pub(crate) home: PathBuf,
    pub(crate) source_home: PathBuf,
    pub(crate) trust: crate::extension_trust::Inputs,
    pub(crate) overlays: Vec<String>,
    pub(crate) euid: u32,
}

/// Run `dot test` from an immutable invocation snapshot.
pub(crate) fn run(runtime: &Runtime, args: &[OsString], streams: &mut Streams<'_>) -> i32 {
    match prepare(runtime, args, streams) {
        Ok(Some((context, options, suites))) => {
            crate::test_runner::run(&context, &options, &suites, streams)
        }
        Ok(None) => 0,
        Err((code, message)) => {
            if !message.is_empty() {
                let _ = writeln!(streams.stderr, "{message}");
            }
            code
        }
    }
}

type Failure = (i32, String);
type Prepared<'a> = Option<(Context<'a>, Options, Vec<Suite>)>;

fn prepare<'a>(
    runtime: &'a Runtime,
    args: &[OsString],
    streams: &mut Streams<'_>,
) -> Result<Prepared<'a>, Failure> {
    let config = crate::startup::check(runtime)
        .map_err(|failure| (failure.code(), failure.line().to_string()))?;
    let uid = crate::cleanup::euid();
    let home = canonical(runtime.home());
    let base = crate::repos_base::select(
        runtime,
        &home.to_string_lossy(),
        &runtime.state_home().to_string_lossy(),
        streams.stderr,
    )
    .map_err(|()| (1, String::new()))?;
    // Test differs from doctor: an inspect resolution failure gates all work.
    let mut overlays = crate::overlays::State::default();
    let mut profiles = crate::profiles::State::default();
    let prefix = text(runtime.value("PREFIX"));
    let inputs = crate::overlays::ResolveInputs {
        home: runtime.home().to_string_lossy().into_owned(),
        xdg_config: runtime.config_home().to_string_lossy().into_owned(),
        discovery_silent: false,
        default_profile: Some(config.default_profile.clone()),
        user: crate::profiles::current_user(),
        host: crate::platform::detect_host().ok(),
        platform: crate::platform::detect_platform().ok(),
        termux: prefix.contains("/com.termux/"),
        euid: uid,
    };
    if let Err(error) = crate::overlays::resolve(&mut overlays, &mut profiles, "inspect", &inputs) {
        for warning in &overlays.warnings {
            let _ = writeln!(streams.stderr, "{warning}");
        }
        return Err((1, error.to_string()));
    }
    let source_home = source_home(runtime, &home, &base)?;
    let mut parallel = true;
    let mut verbose = runtime.value("GITHUB_ACTIONS").is_some();
    let mut raw_jobs = runtime
        .value("DOT_TEST_JOBS")
        .unwrap_or_default()
        .to_os_string();
    let mut list = false;
    let mut filters = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_bytes() {
            b"-s" | b"--sequential" => parallel = false,
            b"-v" | b"--verbose" => verbose = true,
            b"-j" | b"--jobs" => {
                raw_jobs = args
                    .next()
                    .ok_or_else(|| (2, format!("missing value for {}", arg.to_string_lossy())))?
                    .clone()
            }
            b"-l" | b"--list" => list = true,
            b"-h" | b"--help" => {
                streams.stdout.write_all(b"usage: dot test [-s|--sequential] [-v|--verbose] [-j N|--jobs N] [--list] [name ...]\n\nSet DOT_TEST_INCLUDE_PROVIDER=1 to include the provider suite in an\nunfiltered run. Select `dot` by name to run only the provider suite.\n").map_err(|_| (1, String::new()))?;
                return Ok(None);
            }
            raw if raw.starts_with(b"--jobs=") => {
                use std::os::unix::ffi::OsStringExt as _;
                raw_jobs = OsString::from_vec(raw[7..].to_vec());
            }
            raw if raw.starts_with(b"-") => {
                return Err((2, format!("unknown option: {}", arg.to_string_lossy())));
            }
            _ => filters.push(arg.to_string_lossy().into_owned()),
        }
    }
    let include_provider = match runtime
        .value("DOT_TEST_INCLUDE_PROVIDER")
        .unwrap_or(OsStr::new("0"))
        .as_bytes()
    {
        b"0" => false,
        b"1" => true,
        _ => return Err((2, "DOT_TEST_INCLUDE_PROVIDER must be 0 or 1".into())),
    };
    let extensions = config.extensions_dir.unwrap_or_default();
    let override_dir = runtime.value("DOT_TEST_TESTS_DIR");
    let tests = if let Some(dir) = override_dir {
        PathBuf::from(dir)
    } else if source_home != home {
        source_home.join(".local/lib/dotfiles/tests")
    } else {
        PathBuf::from(format!("{extensions}/tests"))
    };
    let context = Context {
        runtime,
        home: home.clone(),
        source_home,
        trust: crate::extension_trust::Inputs {
            euid: uid,
            home: home.to_string_lossy().into_owned(),
            extensions_dir: extensions,
            manifest: runtime
                .state_home()
                .join("dot/overlay-links")
                .to_string_lossy()
                .into_owned(),
            retiring_root: String::new(),
        },
        overlays: overlays.overlays,
        euid: uid,
    };
    let mut suites = Vec::new();
    if override_dir.is_none() {
        suites.push(Suite {
            name: "dot".into(),
            path: runtime.source_root().join("tests/run"),
            source: Source::Provider,
        });
    }
    if fs::symlink_metadata(&tests).is_ok() {
        let extension = override_dir.is_none() && context.source_home == home;
        let directory_ok = if extension {
            crate::extension_trust::root_validate(&context.trust.extensions_dir, uid)
                && crate::extension_trust::directory_validate(
                    &tests,
                    &context.trust.extensions_dir,
                    uid,
                )
        } else {
            fs::symlink_metadata(&tests).is_ok_and(|meta| meta.is_dir())
                && crate::extension_trust::directory_stat(&tests, uid)
        };
        if !directory_ok {
            return Err((
                1,
                format!(
                    "dot: unsafe test {} directory: {}",
                    if extension { "extension" } else { "suite" },
                    tests.display()
                ),
            ));
        }
        let mut paths: Vec<_> = fs::read_dir(&tests)
            .map_err(|_| {
                (
                    1,
                    format!("dot: unsafe test suite directory: {}", tests.display()),
                )
            })?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                let name = path.file_name().unwrap_or_default().as_bytes();
                !name.starts_with(b".") && name.ends_with(b"-test")
            })
            .collect();
        paths.sort();
        for path in paths {
            let source = if extension {
                Source::Extension
            } else {
                Source::Local
            };
            let file_name = path.file_name().unwrap_or_default().to_string_lossy();
            let name = file_name
                .strip_suffix("-test")
                .unwrap_or_default()
                .to_string();
            let suite = Suite { name, path, source };
            if !valid(&context, &suite) {
                return Err((
                    1,
                    format!(
                        "dot: unsafe test {}: {}",
                        if extension { "extension" } else { "suite" },
                        suite.path.display()
                    ),
                ));
            }
            if !executable(&suite.path) {
                return Err((
                    1,
                    format!(
                        "dot: test extension is not executable: {}",
                        suite.path.display()
                    ),
                ));
            }
            if !is_valid_suite_identity(&suite.name) {
                return Err((
                    1,
                    format!(
                        "dot: invalid or reserved test identity: {}",
                        suite.path.file_name().unwrap_or_default().to_string_lossy()
                    ),
                ));
            }
            suites.push(suite);
        }
    }
    if list {
        if suites.is_empty() {
            let _ = writeln!(streams.stdout);
        }
        for suite in suites {
            writeln!(streams.stdout, "{}", suite.name).map_err(|_| (1, String::new()))?;
        }
        return Ok(None);
    }
    if filters.is_empty() {
        suites.retain(|suite| suite.source != Source::Provider || include_provider);
        if parallel {
            suites.sort_by_key(|suite| !runs_early_path(&suite.path));
        }
    } else {
        let mut selected = Vec::new();
        let mut seen = BTreeSet::new();
        for filter in filters {
            let mut matched = false;
            for (index, suite) in suites.iter().enumerate() {
                if filter_matches(&suite.name, &filter) {
                    matched = true;
                    if seen.insert(index) {
                        selected.push(index);
                    }
                }
            }
            if !matched {
                return Err((2, format!("unknown test: {filter}")));
            }
        }
        let mut available: BTreeMap<_, _> = suites.into_iter().enumerate().collect();
        suites = selected
            .into_iter()
            .filter_map(|index| available.remove(&index))
            .collect();
    }
    if suites.is_empty() {
        return Err((1, format!("no tests found in {}", tests.display())));
    }
    let default = automatic_jobs(runtime);
    let jobs = resolve_jobs(
        &raw_jobs.to_string_lossy(),
        suites.len(),
        crate::test_suites::default_jobs(default.as_deref()),
    )
    .ok_or_else(|| {
        (
            2,
            format!("invalid jobs value: {}", raw_jobs.to_string_lossy()),
        )
    })?;
    let color = runtime.value("DOT_TEST_NO_COLOR") != Some(OsStr::new("1"));
    let options = Options {
        parallel,
        verbose,
        jobs,
        requested_jobs: raw_jobs,
        color,
        child_style: child_style(
            color,
            crate::ui::find_gum(&text(runtime.value("PATH"))).is_some(),
            streams.stdout_is_terminal(),
        ),
        ci: runtime.value("GITHUB_ACTIONS").is_some(),
    };
    Ok(Some((context, options, suites)))
}

pub(crate) fn valid(context: &Context<'_>, suite: &Suite) -> bool {
    match suite.source {
        Source::Provider => {
            suite.path == context.runtime.source_root().join("tests/run")
                && executable(&suite.path)
                && crate::extension_trust::file_stat(&suite.path, context.euid)
        }
        Source::Local => {
            fs::symlink_metadata(&suite.path).is_ok_and(|meta| meta.is_file())
                && executable(&suite.path)
                && crate::extension_trust::file_stat(&suite.path, context.euid)
        }
        Source::Extension => {
            crate::extension_trust::file_validate(&suite.path, &context.trust, &context.overlays)
        }
    }
}

fn executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.mode() & 0o111 != 0)
}

fn runs_early_path(path: &Path) -> bool {
    fs::File::open(path)
        .map(std::io::BufReader::new)
        .and_then(crate::test_suites::runs_early_reader)
        .unwrap_or(false)
}

fn child_style(color: bool, has_gum: bool, stdout_terminal: bool) -> bool {
    color && (has_gum || stdout_terminal)
}

fn automatic_jobs(runtime: &Runtime) -> Option<String> {
    let mut count = probe(runtime, "getconf", &["_NPROCESSORS_ONLN"]);
    if count.as_deref().is_none_or(str::is_empty)
        && probe(runtime, "uname", &["-s"]).as_deref() == Some("Darwin")
    {
        count = probe(runtime, "sysctl", &["-n", "hw.ncpu"]);
    }
    count
}

fn probe(runtime: &Runtime, name: &str, args: &[&str]) -> Option<String> {
    let program = runtime.find_on_path(name)?;
    let output = Command::new(program)
        .args(args)
        .env_clear()
        .envs(runtime.env())
        .current_dir(runtime.cwd())
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output.status.success().then(|| {
        let mut value = output.stdout.as_slice();
        while value.last() == Some(&b'\n') {
            value = &value[..value.len() - 1];
        }
        String::from_utf8_lossy(value).into_owned()
    })
}
fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
fn text(value: Option<&OsStr>) -> String {
    value.unwrap_or_default().to_string_lossy().into_owned()
}

fn source_home(
    runtime: &Runtime,
    home: &Path,
    base: &crate::repos_base::Base,
) -> Result<PathBuf, Failure> {
    let selected = runtime
        .value("DOT_TEST_SOURCE_HOME")
        .map(PathBuf::from)
        .map(|path| canonical(&path))
        .or_else(|| {
            git(
                runtime,
                &[OsString::from("-C"), runtime.cwd().as_os_str().to_owned()],
                &["rev-parse", "--show-toplevel"],
            )
            .map(OsString::from_vec)
            .map(PathBuf::from)
            .map(|path| canonical(&path))
            .filter(|path| tree_ok(path) && matches_base(runtime, path, base))
        })
        .unwrap_or_else(|| home.to_path_buf());
    if selected != home {
        if !tree_ok(&selected) {
            return Err((
                2,
                format!("dot test: invalid source home: {}", selected.display()),
            ));
        }
        if !matches_base(runtime, &selected, base) {
            return Err((
                2,
                format!(
                    "dot test: source home does not match the configured base repository: {}",
                    selected.display()
                ),
            ));
        }
    }
    Ok(selected)
}

fn tree_ok(path: &Path) -> bool {
    path.join(".local/lib/dotfiles/tests").is_dir()
        && path.join(".local/lib/dotfiles/tests/helpers.sh").is_file()
}

fn matches_base(runtime: &Runtime, path: &Path, base: &crate::repos_base::Base) -> bool {
    let Some(prefix) = base.git_prefix() else {
        return false;
    };
    let Some(candidate) = git(
        runtime,
        &[OsString::from("-C"), path.as_os_str().to_owned()],
        &["rev-parse", "--git-common-dir"],
    ) else {
        return false;
    };
    let Some(client) = git(runtime, &prefix, &["rev-parse", "--git-common-dir"]) else {
        return false;
    };
    if canonical(&path.join(OsStr::from_bytes(&candidate)))
        != canonical(&runtime.home().join(OsStr::from_bytes(&client)))
    {
        return false;
    }
    git(runtime, &prefix, &["worktree", "list", "--porcelain"]).is_some_and(|list| {
        list.split(|byte| *byte == b'\n')
            .filter_map(|line| line.strip_prefix(b"worktree "))
            .any(|registered| canonical(Path::new(OsStr::from_bytes(registered))) == path)
    })
}

fn git(runtime: &Runtime, prefix: &[OsString], args: &[&str]) -> Option<Vec<u8>> {
    let program = runtime.find_on_path("git")?;
    let mut command = Command::new(program);
    command
        .args(prefix)
        .args(args)
        .env_clear()
        .envs(runtime.env())
        .current_dir(runtime.cwd())
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    crate::temp::sanitize_git_env(&mut command);
    let output = command.output().ok()?;
    output.status.success().then(|| {
        let mut bytes = output.stdout;
        while bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        bytes
    })
}

/// Prepare the deterministic host Git backend and each suite's environment.
pub(crate) fn environment(
    context: &Context<'_>,
    options: &Options,
    suite: &Suite,
    root: &Path,
    temporary: &Path,
    result: &Path,
) -> Result<BTreeMap<OsString, OsString>, Failure> {
    let runtime = context.runtime;
    let host = &context.home;
    let source = &context.source_home;
    let backend = root.join("system-git");
    let mut candidates: Vec<PathBuf> = runtime
        .value("DOT_TEST_SYSTEM_GIT")
        .map(PathBuf::from)
        .into_iter()
        .collect();
    candidates.extend(["/usr/bin/git", "/bin/git", "/opt/homebrew/bin/git"].map(PathBuf::from));
    if let Some(prefix) = runtime.value("PREFIX") {
        candidates.push(Path::new(prefix).join("bin/git"));
    }
    let system_git = candidates
        .into_iter()
        .find(|path| {
            path.is_absolute()
                && executable(path)
                && [source, host].iter().all(|home| {
                    let rejected = home.join(".local/bin/git");
                    match (fs::metadata(path), fs::metadata(rejected)) {
                        (Ok(a), Ok(b)) => (a.dev(), a.ino()) != (b.dev(), b.ino()),
                        _ => true,
                    }
                })
        })
        .ok_or_else(|| (2, "dot test: could not find a system Git executable".into()))?;
    if !backend.exists() {
        fs::create_dir(&backend)
            .and_then(|()| std::os::unix::fs::symlink(&system_git, backend.join("git")))
            .map_err(|_| {
                (
                    2,
                    "dot test: could not configure system Git executable".into(),
                )
            })?;
    }
    let mut path = runtime.value("PATH").unwrap_or_default().to_os_string();
    let source_bin = source.join(".local/bin");
    let mut paths: Vec<_> = std::env::split_paths(&path).collect();
    if paths.first() == Some(&source_bin) {
        paths.remove(0);
    }
    paths.insert(0, backend);
    paths.insert(0, source_bin);
    let provider = suite.source == Source::Provider && source != host;
    if source != host {
        paths.push(host.join(".local/bin"));
    }
    if provider {
        paths.remove(0);
    }
    path = std::env::join_paths(paths).map_err(|_| (2, "dot test: invalid PATH".into()))?;
    let child_home = if provider { host } else { source };
    let mut env = runtime.env().clone();
    env.remove(OsStr::new("DOT_CLIENT_GIT_DIR"));
    env.remove(OsStr::new("DOT_TEST_TIMEOUT_EXPIRED_FILE"));
    for (key, value) in [
        ("HOME", child_home.as_os_str().to_owned()),
        ("PATH", path),
        ("TMPDIR", temporary.as_os_str().to_owned()),
        (
            "XDG_CACHE_HOME",
            temporary.join("xdg-cache").into_os_string(),
        ),
        (
            "XDG_STATE_HOME",
            temporary.join("xdg-state").into_os_string(),
        ),
        ("DOT_TEST", "1".into()),
        (
            "DOT_TEST_STYLE",
            if options.child_style { "1" } else { "0" }.into(),
        ),
        ("DOT_TEST_JOBS", options.jobs.to_string().into()),
        (
            "DOT_TEST_PARALLEL",
            if options.parallel { "1" } else { "0" }.into(),
        ),
        (
            "DOT_TEST_DOT_ROOT",
            runtime.source_root().as_os_str().to_owned(),
        ),
        ("DOT_TEST_HOST_HOME", host.as_os_str().to_owned()),
        ("DOT_TEST_SOURCE_HOME", child_home.as_os_str().to_owned()),
        ("DOT_TEST_RESULT_FILE", result.as_os_str().to_owned()),
        ("DOT_TEST_SYSTEM_GIT", system_git.into_os_string()),
        (
            "DOT_TEST_REPORTER",
            runtime
                .source_root()
                .join("lib/dot/public/test-reporter-v1")
                .into_os_string(),
        ),
        (
            "DOT_TEST_TIMEOUT",
            runtime
                .source_root()
                .join("lib/dot/public/test-timeout-v1")
                .into_os_string(),
        ),
    ] {
        env.insert(key.into(), value);
    }
    if source != host {
        for (key, suffix) in [
            ("MISE_DATA_DIR", ".local/share/mise"),
            ("MISE_STATE_DIR", ".local/state/mise"),
            ("MISE_CACHE_DIR", ".cache/mise"),
        ] {
            env.entry(key.into())
                .or_insert_with(|| host.join(suffix).into_os_string());
        }
    }
    if suite.source == Source::Provider {
        env.insert(
            "DOT_TEST_REQUESTED_JOBS".into(),
            options.requested_jobs.clone(),
        );
    }
    Ok(env)
}

pub(crate) fn label(suite: &Suite) -> String {
    suite_label(&suite.name)
}

#[cfg(test)]
mod tests {
    use super::child_style;

    #[test]
    fn terminal_enables_child_style_without_gum() {
        assert!(child_style(true, false, true));
        assert!(child_style(true, true, false));
        assert!(!child_style(false, true, true));
        assert!(!child_style(true, false, false));
    }
}

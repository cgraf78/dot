//! Test-coordinator scheduling decisions from `lib/dot/test.sh`,
//! `lib/dot/test/runner.sh`, and `lib/dot/test/discovery.sh`.
//!
//! Pure decision helpers behind `dot test`: per-source suite
//! timeouts, result-record classification (a zero exit alone never
//! proves success), worker-count selection and validation, the
//! early-wave scheduling marker, suite labels, the summary line,
//! suite-identity validation, and name-filter matching. Everything
//! is a pure function of explicit inputs — shell globals (`$HOME`,
//! option state, the discovered script arrays) stay with the caller
//! so tests inject fixtures deterministically.
//!
//! Process orchestration (parallel/sequential scheduling, suite
//! sandboxing, output multiplexing, rendering), temporary-root
//! lifecycle and cancellation, source-home and Git-backend
//! selection, and directory inventory stay in shell: they depend on
//! job control, process groups, and worktree trust checks with no
//! faithful pure model.

/// Terminal classification of one finished suite, mirroring the
/// words `_classify_suite` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuiteClassification {
    /// Zero exit plus a well-formed `complete` record with no failures.
    Pass,
    /// Nonzero exit, or a well-formed `complete` record with failures.
    Fail,
    /// Zero exit plus a well-formed `skip` record.
    Skip,
    /// Zero exit but the result record is missing or empty: the suite
    /// finished without proving anything (a false green).
    Incomplete,
    /// Zero exit with a malformed result record.
    Invalid,
}

impl SuiteClassification {
    /// The word the shell classifier prints for this outcome.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Skip => "skip",
            Self::Incomplete => "incomplete",
            Self::Invalid => "invalid",
        }
    }
}

/// Skip one run of tab separators, like the shell moving past an
/// `IFS=$'\t'` delimiter run between `read` fields.
fn skip_tabs(mut rest: &[u8]) -> &[u8] {
    while rest.first() == Some(&b'\t') {
        rest = &rest[1..];
    }
    rest
}

/// Split one result line the way `IFS=$'\t' read -r kind first
/// second` does: leading tab runs vanish, the first two fields end
/// at the next tab run, and every remaining byte (minus trailing
/// tab runs, which are trailing `IFS` whitespace) rejoins onto the
/// last variable with its original separators intact.
fn read_fields(line: &[u8]) -> (&[u8], &[u8], &[u8]) {
    fn word(rest: &[u8]) -> (&[u8], &[u8]) {
        match rest.iter().position(|byte| *byte == b'\t') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, &[]),
        }
    }
    let (kind, rest) = word(skip_tabs(line));
    let (first, rest) = word(skip_tabs(rest));
    let mut second = skip_tabs(rest);
    while second.last() == Some(&b'\t') {
        second = &second[..second.len() - 1];
    }
    (kind, first, second)
}

/// Whether `value` matches the shell's `^(0|[1-9][0-9]*)$` count
/// gate: no sign, no leading zeros, no length bound (comparison
/// stays lexical, so arbitrarily long counts never overflow).
fn is_count(value: &[u8]) -> bool {
    if value == b"0" {
        return true;
    }
    let mut bytes = value.iter();
    match bytes.next() {
        Some(first) if (b'1'..=b'9').contains(first) => {}
        _ => return false,
    }
    bytes.all(|byte| byte.is_ascii_digit())
}

/// `_classify_suite`: classify a finished suite from its exit code
/// and its private result record (`None` when the record is
/// missing). A nonzero exit is failure regardless of the record;
/// a zero exit still needs exactly one newline-terminated line
/// holding either a `complete` record with two counts (pass when
/// the failure count is zero) or a `skip` record with an empty
/// second field.
pub fn classify_suite(exit_code: i32, result: Option<&[u8]>) -> SuiteClassification {
    if exit_code != 0 {
        return SuiteClassification::Fail;
    }
    let Some(content) = result else {
        return SuiteClassification::Incomplete;
    };
    if content.is_empty() {
        return SuiteClassification::Incomplete;
    }
    // `wc -l` counts newlines (so exactly one is required) and the
    // `tail -c 1 | od` probe requires that newline to terminate the
    // line rather than merely appear inside it.
    if content.iter().filter(|byte| **byte == b'\n').count() != 1 || content.last() != Some(&b'\n')
    {
        return SuiteClassification::Invalid;
    }
    let (kind, first, second) = read_fields(&content[..content.len() - 1]);
    if kind == b"complete" && is_count(first) && is_count(second) {
        if second == b"0" {
            SuiteClassification::Pass
        } else {
            SuiteClassification::Fail
        }
    } else if kind == b"skip" && second.is_empty() {
        SuiteClassification::Skip
    } else {
        SuiteClassification::Invalid
    }
}

/// `_dot_test_suite_timeout`: per-source suite timeout in seconds.
/// A non-empty `DOT_TEST_SUITE_TIMEOUT_SECONDS` override wins
/// verbatim (even when it is not numeric, like the shell's
/// `printf '%s\n'`); otherwise the provider corpus gets 900, local
/// repository verification gets 600, and anything else gets 300.
pub fn suite_timeout(source: &str, override_seconds: Option<&str>) -> String {
    if let Some(value) = override_seconds.filter(|value| !value.is_empty()) {
        return value.to_string();
    }
    match source {
        "provider" => "900",
        "local" => "600",
        _ => "300",
    }
    .to_string()
}

/// `_default_jobs`: map one processor-count probe to a worker
/// count. `None` covers both the missing-`getconf` and the
/// non-numeric-output cases (the caller picks the `sysctl` fallback
/// on Darwin exactly like the shell); overlong probes fall back
/// too, zero clamps to one worker, and automatic selection never
/// exceeds the 24-worker cap (explicit `-j` values bypass this
/// function entirely).
pub fn default_jobs(nproc_text: Option<&str>) -> u32 {
    let Some(text) = nproc_text else {
        return 4;
    };
    if text.is_empty() || text.len() > 9 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return 4;
    }
    let Ok(value) = text.parse::<u32>() else {
        return 4;
    };
    value.clamp(1, 24)
}

/// `_dot_test_runs_early`: whether a suite opts into the first
/// worker wave with an exact `# dot-suite-priority: early` header
/// line. Only the first 20 lines count, so discovery cost stays
/// independent of suite size and fixture text never reads as
/// scheduling metadata.
pub fn runs_early(script_bytes: &[u8]) -> bool {
    script_bytes
        .split(|byte| *byte == b'\n')
        .take(20)
        .any(|line| line == b"# dot-suite-priority: early")
}

/// `_dot_test_suite_label`: display label for a discovered suite
/// identity — the reserved `dot` provider identity prints bare,
/// every extension prints with its `-test` suffix.
pub fn suite_label(identity: &str) -> String {
    if identity == "dot" {
        "dot".to_string()
    } else {
        format!("{identity}-test")
    }
}

/// `runner.sh` jobs preamble plus validation and clamping: an
/// empty request fills from [`default_jobs`] first (the
/// `[[ -z "$max_jobs" ]]` preamble, so it never fails), then
/// non-numeric, leading-zero, and overlong values are rejected
/// (the shell exits 2 with `invalid jobs value`), and the survivor
/// clamps into one worker at the bottom and the discovered suite
/// count at the top.
pub fn resolve_jobs(raw: &str, suite_count: usize, default: u32) -> Option<usize> {
    if raw.is_empty() {
        let filled = default as usize;
        return Some(filled.max(1).min(suite_count));
    }
    if raw.len() > 9 || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if raw.len() > 1 && raw.starts_with('0') {
        return None;
    }
    let value: usize = raw.parse().ok()?;
    Some(value.max(1).min(suite_count))
}

/// `runner.sh` summary line: `Suites: {passed} passed` plus the
/// skipped/failed clauses only when nonzero, then the total.
pub fn format_summary(passed: u64, skipped: u64, failed: u64, total: usize) -> String {
    let mut summary = format!("Suites: {passed} passed");
    if skipped > 0 {
        summary.push_str(&format!(", {skipped} skipped"));
    }
    if failed > 0 {
        summary.push_str(&format!(", {failed} failed"));
    }
    summary.push_str(&format!(" ({total} total)"));
    summary
}

/// `discovery.sh` suite-identity gate: start lowercase ASCII,
/// continue with lowercase ASCII alphanumerics or dashes, and never
/// the reserved `dot` provider identity.
pub fn is_valid_suite_identity(name: &str) -> bool {
    if name == "dot" {
        return false;
    }
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// `discovery.sh` name-filter match: a filter selects the suite it
/// names exactly plus that family's dash-prefixed members (`core`
/// selects `core` and `core-extra`, never `coreutils`).
pub fn filter_matches(identity: &str, filter: &str) -> bool {
    identity == filter
        || identity
            .strip_prefix(filter)
            .is_some_and(|rest| rest.starts_with('-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_override_wins_verbatim() {
        assert_eq!(suite_timeout("provider", Some("45")), "45");
        assert_eq!(suite_timeout("local", Some("fast")), "fast");
        assert_eq!(suite_timeout("local", Some("")), "600");
        assert_eq!(suite_timeout("provider", None), "900");
        assert_eq!(suite_timeout("local", None), "600");
        assert_eq!(suite_timeout("extension", None), "300");
    }

    #[test]
    fn jobs_selection_clamps_and_falls_back() {
        assert_eq!(default_jobs(Some("8")), 8);
        assert_eq!(default_jobs(Some("007")), 7);
        assert_eq!(default_jobs(Some("0")), 1);
        assert_eq!(default_jobs(Some("100")), 24);
        assert_eq!(default_jobs(Some("abc")), 4);
        assert_eq!(default_jobs(None), 4);
        assert_eq!(default_jobs(Some("1234567890")), 4);
    }

    #[test]
    fn jobs_resolution_rejects_and_clamps() {
        assert_eq!(resolve_jobs("4", 3, 8), Some(3));
        assert_eq!(resolve_jobs("9", 3, 8), Some(3));
        assert_eq!(resolve_jobs("10", 9, 8), Some(9));
        assert_eq!(resolve_jobs("007", 9, 8), None);
        assert_eq!(resolve_jobs("0", 5, 8), Some(1));
        assert_eq!(resolve_jobs("", 3, 8), Some(3));
        assert_eq!(resolve_jobs("01", 3, 8), None);
        assert_eq!(resolve_jobs("abc", 3, 8), None);
    }

    #[test]
    fn identity_and_filter_gates() {
        assert!(is_valid_suite_identity("core"));
        assert!(!is_valid_suite_identity("dot"));
        assert!(!is_valid_suite_identity("0abc"));
        assert!(!is_valid_suite_identity("a_b"));
        assert!(filter_matches("core-extra", "core"));
        assert!(!filter_matches("coreutils", "core"));
    }

    #[test]
    fn summary_omits_zero_clauses() {
        assert_eq!(format_summary(2, 0, 0, 2), "Suites: 2 passed (2 total)");
        assert_eq!(
            format_summary(1, 2, 3, 6),
            "Suites: 1 passed, 2 skipped, 3 failed (6 total)"
        );
    }
}

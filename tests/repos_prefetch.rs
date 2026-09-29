//! Decision contract for [`dot::repos_prefetch::proves_current`] and the
//! advertisement parser. Each case isolates one reason a skipped fetch could
//! differ from a real one; the proof must refuse every such case.

use dot::repos_prefetch::{Probe, config_fingerprint, parse_advertisement, proves_current};
use std::collections::BTreeMap;
use std::time::Instant;

const MAIN: &str = "1111111111111111111111111111111111111111";
const DEV: &str = "2222222222222222222222222222222222222222";
const TAG: &str = "3333333333333333333333333333333333333333";
const OTHER: &str = "4444444444444444444444444444444444444444";
/// Scoped config bytes in `git config -z --list --show-scope` shape: one
/// `scope NUL key LF value NUL` record per `(scope, entry)`.
fn scoped(entries: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (scope, entry) in entries {
        out.extend_from_slice(scope.as_bytes());
        out.push(0);
        out.extend_from_slice(entry.as_bytes());
        out.push(0);
    }
    out
}

/// Local-scope origin with the default refspec, plus `extra` local entries.
fn config(extra: &[&str]) -> Vec<u8> {
    let mut entries = vec![
        ("local", "remote.origin.url\nfile:///remote.git"),
        (
            "local",
            "remote.origin.fetch\n+refs/heads/*:refs/remotes/origin/*",
        ),
    ];
    entries.extend(extra.iter().map(|entry| ("local", *entry)));
    scoped(&entries)
}

fn probe_with(config: Vec<u8>) -> Probe {
    Probe {
        upstream: "origin/main".to_string(),
        config: config_fingerprint(&config).expect("fingerprint"),
        ssh_config: None,
        heads: BTreeMap::from([
            ("main".to_string(), MAIN.to_string()),
            ("dev".to_string(), DEV.to_string()),
        ]),
        tags: BTreeMap::from([("v1".to_string(), TAG.to_string())]),
        observed: Instant::now(),
    }
}

/// Local refs that exactly mirror [`probe_with`], including remote HEAD.
fn current_refs() -> String {
    format!(
        "{MAIN} refs/remotes/origin/HEAD\n{DEV} refs/remotes/origin/dev\n{MAIN} refs/remotes/origin/main\n{TAG} refs/tags/v1\n"
    )
}

fn check(probe: &Probe, config: &[u8], refs: &str) -> bool {
    proves_current(probe, "origin/main", config, None, refs)
}

#[test]
fn mirrored_refs_prove_the_fetch_is_a_no_op() {
    let bytes = config(&[]);
    assert!(check(&probe_with(bytes.clone()), &bytes, &current_refs()));
}

#[test]
fn a_moved_branch_requires_a_fetch() {
    let bytes = config(&[]);
    let refs = current_refs().replace(
        &format!("{DEV} refs/remotes/origin/dev"),
        &format!("{OTHER} refs/remotes/origin/dev"),
    );
    assert!(!check(&probe_with(bytes.clone()), &bytes, &refs));
}

#[test]
fn a_new_remote_branch_requires_a_fetch() {
    let bytes = config(&[]);
    let mut probe = probe_with(bytes.clone());
    probe.heads.insert("feature".to_string(), OTHER.to_string());
    assert!(!check(&probe, &bytes, &current_refs()));
}

#[test]
fn a_missing_upstream_branch_requires_a_fetch() {
    let bytes = config(&[]);
    let mut probe = probe_with(bytes.clone());
    probe.heads.remove("main");
    let refs = current_refs().replace(&format!("{MAIN} refs/remotes/origin/main\n"), "");
    assert!(!check(&probe, &bytes, &refs));
}

#[test]
fn a_changed_upstream_voids_the_probe() {
    let bytes = config(&[]);
    let probe = probe_with(bytes.clone());
    assert!(!proves_current(
        &probe,
        "origin/dev",
        &bytes,
        None,
        &current_refs()
    ));
}

#[test]
fn changed_git_config_voids_the_probe() {
    let probe = probe_with(config(&[]));
    let now = config(&["url.file:///other.git.insteadof\nfile:///remote.git"]);
    assert!(!check(&probe, &now, &current_refs()));
}

#[test]
fn changed_ssh_config_voids_the_probe() {
    let bytes = config(&[]);
    let mut probe = probe_with(bytes.clone());
    probe.ssh_config = Some(b"Host a\n".to_vec());
    assert!(!proves_current(
        &probe,
        "origin/main",
        &bytes,
        Some(b"Host b\n"),
        &current_refs()
    ));
    assert!(!proves_current(
        &probe,
        "origin/main",
        &bytes,
        None,
        &current_refs()
    ));
    assert!(proves_current(
        &probe,
        "origin/main",
        &bytes,
        Some(b"Host a\n"),
        &current_refs()
    ));
}

#[test]
fn stale_tracking_refs_are_fine_without_prune() {
    let bytes = config(&[]);
    let refs = format!("{}{OTHER} refs/remotes/origin/gone\n", current_refs());
    assert!(check(&probe_with(bytes.clone()), &bytes, &refs));
}

#[test]
fn stale_tracking_refs_require_a_fetch_under_remote_prune() {
    let bytes = config(&["remote.origin.prune\ntrue"]);
    let refs = format!("{}{OTHER} refs/remotes/origin/gone\n", current_refs());
    assert!(!check(&probe_with(bytes.clone()), &bytes, &refs));
}

#[test]
fn stale_tracking_refs_require_a_fetch_under_global_prune() {
    let bytes = config(&["fetch.prune"]);
    let refs = format!("{}{OTHER} refs/remotes/origin/gone\n", current_refs());
    assert!(!check(&probe_with(bytes.clone()), &bytes, &refs));
}

#[test]
fn remote_prune_false_overrides_global_prune() {
    let bytes = config(&["fetch.prune\ntrue", "remote.origin.prune\nfalse"]);
    let refs = format!("{}{OTHER} refs/remotes/origin/gone\n", current_refs());
    assert!(check(&probe_with(bytes.clone()), &bytes, &refs));
}

#[test]
fn an_unparsable_prune_value_is_not_guessed() {
    let bytes = config(&["fetch.prune\nsometimes"]);
    assert!(!check(&probe_with(bytes.clone()), &bytes, &current_refs()));
}

#[test]
fn prune_tags_is_not_modeled() {
    let bytes = config(&["fetch.prunetags\ntrue"]);
    assert!(!check(&probe_with(bytes.clone()), &bytes, &current_refs()));
}

#[test]
fn a_custom_refspec_is_not_modeled() {
    let bytes = scoped(&[
        ("local", "remote.origin.url\nfile:///remote.git"),
        (
            "local",
            "remote.origin.fetch\n+refs/heads/main:refs/remotes/origin/main",
        ),
    ]);
    assert!(!check(&probe_with(bytes.clone()), &bytes, &current_refs()));
}

#[test]
fn an_additional_refspec_is_not_modeled() {
    let bytes = config(&["remote.origin.fetch\n+refs/notes/*:refs/notes/*"]);
    assert!(!check(&probe_with(bytes.clone()), &bytes, &current_refs()));
}

#[test]
fn a_missing_local_tag_requires_a_fetch() {
    let bytes = config(&[]);
    let refs = current_refs().replace(&format!("{TAG} refs/tags/v1\n"), "");
    assert!(!check(&probe_with(bytes.clone()), &bytes, &refs));
}

#[test]
fn a_different_local_tag_requires_a_fetch() {
    let bytes = config(&[]);
    let refs = current_refs().replace(
        &format!("{TAG} refs/tags/v1"),
        &format!("{OTHER} refs/tags/v1"),
    );
    assert!(!check(&probe_with(bytes.clone()), &bytes, &refs));
}

#[test]
fn no_tags_ignores_tag_differences() {
    let bytes = config(&["remote.origin.tagopt\n--no-tags"]);
    let refs = current_refs().replace(&format!("{TAG} refs/tags/v1\n"), "");
    assert!(check(&probe_with(bytes.clone()), &bytes, &refs));
}

#[test]
fn forced_tag_fetching_is_not_modeled() {
    let bytes = config(&["remote.origin.tagopt\n--tags"]);
    assert!(!check(&probe_with(bytes.clone()), &bytes, &current_refs()));
}

#[test]
fn a_missing_remote_head_requires_a_fetch_that_may_create_it() {
    let bytes = config(&[]);
    let refs = current_refs().replace(&format!("{MAIN} refs/remotes/origin/HEAD\n"), "");
    assert!(!check(&probe_with(bytes.clone()), &bytes, &refs));
}

#[test]
fn a_missing_remote_head_is_fine_when_the_policy_never_writes_it() {
    let bytes = config(&["remote.origin.followremotehead\nnever"]);
    let refs = current_refs().replace(&format!("{MAIN} refs/remotes/origin/HEAD\n"), "");
    assert!(check(&probe_with(bytes.clone()), &bytes, &refs));
}

#[test]
fn remote_head_policies_that_write_or_warn_are_not_modeled() {
    for policy in ["always", "warn", "warn-if-not-main"] {
        let entry = format!("remote.origin.followremotehead\n{policy}");
        let bytes = config(&[entry.as_str()]);
        assert!(
            !check(&probe_with(bytes.clone()), &bytes, &current_refs()),
            "{policy}"
        );
    }
}

#[test]
fn recursive_submodule_fetching_is_not_modeled() {
    for extra in [
        "fetch.recursesubmodules\ntrue",
        "fetch.recursesubmodules",
        "submodule.recurse\ntrue",
    ] {
        let bytes = config(&[extra]);
        assert!(
            !check(&probe_with(bytes.clone()), &bytes, &current_refs()),
            "{extra:?}"
        );
    }
    let bytes = config(&["fetch.recursesubmodules\non-demand"]);
    assert!(check(&probe_with(bytes.clone()), &bytes, &current_refs()));
}

#[test]
fn malformed_local_refs_refuse_the_proof() {
    let bytes = config(&[]);
    assert!(!check(
        &probe_with(bytes.clone()),
        &bytes,
        "not-a-ref-line\n"
    ));
}

#[test]
fn advertisement_parsing_keeps_unpeeled_tags_and_branches() {
    let text = format!(
        "{MAIN}\trefs/heads/main\n{DEV}\trefs/heads/team/dev\n{TAG}\trefs/tags/v1\n{MAIN}\trefs/tags/v1^{{}}\n"
    );
    let (heads, tags) = parse_advertisement(&text).expect("parse");
    assert_eq!(heads.get("main").map(String::as_str), Some(MAIN));
    assert_eq!(heads.get("team/dev").map(String::as_str), Some(DEV));
    assert_eq!(tags.get("v1").map(String::as_str), Some(TAG));
    assert_eq!(tags.len(), 1);
}

#[test]
fn advertisement_parsing_accepts_an_empty_remote() {
    let (heads, tags) = parse_advertisement("").expect("parse");
    assert!(heads.is_empty() && tags.is_empty());
}

#[test]
fn advertisement_parsing_rejects_unexpected_lines() {
    assert!(parse_advertisement("garbage\n").is_none());
    assert!(parse_advertisement(&format!("{MAIN}\tHEAD\n")).is_none());
    assert!(parse_advertisement(&format!("{MAIN}\trefs/pull/1/head\n")).is_none());
    assert!(parse_advertisement("zz\trefs/heads/main\n").is_none());
}

#[test]
fn per_invocation_command_scope_values_do_not_void_the_probe() {
    let at_probe = [
        config(&[]),
        scoped(&[("command", "http.extraheader\nid: 1")]),
    ]
    .concat();
    let now = [
        config(&[]),
        scoped(&[("command", "http.extraheader\nid: 2")]),
    ]
    .concat();
    assert!(check(&probe_with(at_probe), &now, &current_refs()));
}

#[test]
fn command_scope_settings_still_drive_the_model() {
    let bytes = [config(&[]), scoped(&[("command", "fetch.prune\ntrue")])].concat();
    let refs = format!("{}{OTHER} refs/remotes/origin/gone\n", current_refs());
    assert!(!check(&probe_with(bytes.clone()), &bytes, &refs));
}

#[test]
fn unparsable_config_listing_refuses_the_proof() {
    let bytes = config(&[]);
    let probe = probe_with(bytes.clone());
    // A scope with no entry, and bytes that are not UTF-8.
    for broken in [&b"local"[..], &b"local\0\xff\0"[..]] {
        assert!(config_fingerprint(broken).is_none(), "{broken:?}");
        assert!(!check(&probe, broken, &current_refs()), "{broken:?}");
    }
}

#[test]
fn bundle_uris_are_not_modeled() {
    let bytes = config(&["fetch.bundleuri\nhttps://bundles.example.invalid/repo"]);
    assert!(!check(&probe_with(bytes.clone()), &bytes, &current_refs()));
}

#[test]
fn other_command_scope_differences_void_the_probe() {
    let at_probe = [
        config(&[]),
        scoped(&[("command", "url.file:///a.git.insteadof\nfile:///remote.git")]),
    ]
    .concat();
    let now = [
        config(&[]),
        scoped(&[("command", "url.file:///b.git.insteadof\nfile:///remote.git")]),
    ]
    .concat();
    assert!(!check(&probe_with(at_probe), &now, &current_refs()));
}

#[test]
fn unlisted_fetch_and_remote_keys_are_not_modeled() {
    for extra in [
        "fetch.bundlecreationtoken\n7",
        "fetch.somefuturekey\ntrue",
        "fetch.writecommitgraph\ntrue",
        "fetch.output\nbogus",
        "remote.origin.mirror\ntrue",
        "remote.origin.vcs\nhg",
        "remote.origin.promisor\ntrue",
    ] {
        let bytes = config(&[extra]);
        assert!(
            !check(&probe_with(bytes.clone()), &bytes, &current_refs()),
            "{extra:?}"
        );
    }
}

#[test]
fn listed_benign_keys_and_other_remotes_do_not_block_the_proof() {
    for extra in [
        "remote.origin.proxy\nhttp://proxy.example.invalid",
        "remote.upstream.mirror\ntrue",
    ] {
        let bytes = config(&[extra]);
        assert!(
            check(&probe_with(bytes.clone()), &bytes, &current_refs()),
            "{extra:?}"
        );
    }
}

#[test]
fn submodule_recurse_must_be_a_false_boolean() {
    for (value, proves) in [("false", true), ("true", false), ("on-demand", false)] {
        let entry = format!("submodule.recurse\n{value}");
        let bytes = config(&[entry.as_str()]);
        assert_eq!(
            check(&probe_with(bytes.clone()), &bytes, &current_refs()),
            proves,
            "{value}"
        );
    }
}

#[test]
fn file_scope_extra_headers_must_match() {
    let at_probe = config(&["http.extraheader\nAuthorization: Bearer one"]);
    let now = config(&["http.extraheader\nAuthorization: Bearer two"]);
    assert!(!check(&probe_with(at_probe), &now, &current_refs()));
}

#[test]
fn an_overridden_malformed_value_still_refuses_the_proof() {
    // Git parses every occurrence, so an early malformed value fails a real
    // fetch even when a later one is valid.
    for extra in [
        ["fetch.prune\nmaybe", "fetch.prune\nfalse"],
        [
            "remote.origin.tagopt\n--tags",
            "remote.origin.tagopt\n--no-tags",
        ],
        [
            "remote.origin.followremotehead\nalways",
            "remote.origin.followremotehead\ncreate",
        ],
    ] {
        let bytes = config(&extra);
        assert!(
            !check(&probe_with(bytes.clone()), &bytes, &current_refs()),
            "{extra:?}"
        );
    }
}

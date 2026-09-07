//! Native byte contracts for the Shdeps progress renderers.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt as _;

use dot::progress_ui::Palette;
use dot::shdeps_ui_render::{
    Session, Ui, have_jq, print_group_items_with_status, print_group_summaries,
    print_verbose_group_rows, print_verbose_items, prompt_pause, prompt_resume, reset,
};
use dot_test_support::TempDir;

fn palette() -> Palette {
    Palette {
        reset: "<R>".into(),
        bold: "<B>".into(),
        dim: "<D>".into(),
        green: "<G>".into(),
        yellow: "<Y>".into(),
        red: "<E>".into(),
        blue: "<U>".into(),
        cyan: "<C>".into(),
        white: "<W>".into(),
    }
}

fn ui(palette: &Palette, quiet: bool) -> Ui<'_> {
    Ui {
        palette,
        quiet,
        multibyte: false,
    }
}

fn map(rows: &[(Vec<u8>, Vec<u8>)]) -> HashMap<Vec<u8>, Vec<u8>> {
    rows.iter().cloned().collect()
}

fn fallback(group: &[u8]) -> Vec<u8> {
    match group {
        b"packages" => b"Packages".to_vec(),
        b"github-releases" | b"github-repos" => b"GitHub".to_vec(),
        b"cargo" => b"Cargo".to_vec(),
        b"go" => b"Go".to_vec(),
        b"uv" => b"Python tools".to_vec(),
        b"npm" => b"NPM".to_vec(),
        b"custom" => b"Custom".to_vec(),
        b"other" => b"Other".to_vec(),
        _ => group.to_vec(),
    }
}

fn color(status: &[u8]) -> &'static [u8] {
    match status {
        b"ok" => b"<G>",
        b"changed" => b"<U>",
        b"running" => b"<C>",
        b"warning" => b"<Y>",
        b"failed" => b"<E>",
        b"detail" | b"hint" => b"<D>",
        _ => b"",
    }
}

fn padded(bytes: &[u8], width: usize) -> Vec<u8> {
    let mut out = bytes.to_vec();
    out.resize(width.max(bytes.len()), b' ');
    out
}

fn item(live: bool, status: &[u8], name: &[u8], detail: &[u8]) -> Vec<u8> {
    let mut out = if live {
        b"\r\x1b[K".to_vec()
    } else {
        Vec::new()
    };
    out.extend_from_slice(b"  ");
    out.extend_from_slice(color(status));
    out.extend_from_slice(&padded(status, 8));
    out.extend_from_slice(b"<R> ");
    if detail.is_empty() {
        out.extend_from_slice(name);
    } else {
        out.extend_from_slice(&padded(name, 28));
        out.extend_from_slice(b" <D>");
        out.extend_from_slice(detail);
        out.extend_from_slice(b"<R>");
    }
    out.push(b'\n');
    out
}

fn note(live: bool, status: &[u8], detail: &[u8]) -> Vec<u8> {
    let mut out = if live {
        b"\r\x1b[K".to_vec()
    } else {
        Vec::new()
    };
    out.extend_from_slice(b"  ");
    out.extend_from_slice(color(status));
    out.extend_from_slice(&padded(status, 8));
    out.extend_from_slice(b"<R> ");
    out.extend_from_slice(detail);
    out.push(b'\n');
    out
}

fn section(live: bool, label: &[u8]) -> Vec<u8> {
    let mut out = if live {
        b"\r\x1b[K".to_vec()
    } else {
        Vec::new()
    };
    out.extend_from_slice(b"  <B><W>");
    out.extend_from_slice(label);
    out.extend_from_slice(b"<R>\n");
    out
}

fn stage_exec(path: &std::path::Path) {
    std::fs::write(path, b"#!/bin/sh\nexit 0\n").expect("probe");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

#[test]
fn have_jq_rows_are_filesystem_contracts() {
    let fixture = TempDir::new("shdeps-render-jq").expect("fixture");
    let root = fixture.path();
    let exec = root.join("exec");
    let dull = root.join("dull");
    let directory = root.join("directory");
    let linked = root.join("linked");
    let dead = root.join("dead");
    for dir in [&exec, &dull, &directory, &linked, &dead] {
        std::fs::create_dir_all(dir).expect("dir");
    }
    stage_exec(&exec.join("jq"));
    std::fs::write(dull.join("jq"), b"not executable").expect("dull");
    std::fs::create_dir(directory.join("jq")).expect("named dir");
    std::os::unix::fs::symlink(exec.join("jq"), linked.join("jq")).expect("link");
    std::os::unix::fs::symlink(root.join("missing"), dead.join("jq")).expect("dead link");
    assert!(have_jq(exec.to_str().expect("utf8")));
    assert!(have_jq(dull.to_str().expect("utf8")));
    assert!(!have_jq(directory.to_str().expect("utf8")));
    assert!(have_jq(linked.to_str().expect("utf8")));
    assert!(!have_jq(dead.to_str().expect("utf8")));
    assert!(!have_jq(""));
    assert!(!have_jq("/nonexistent-dot-jq-path"));
    assert!(have_jq(&format!(
        "/nonexistent-dot-jq-path:{}",
        exec.display()
    )));
}

#[test]
fn reset_rows_overwrite_every_session_field() {
    for has_jq in [false, true] {
        assert_eq!(
            reset(has_jq),
            Session {
                status: b"ok".to_vec(),
                summary: b"dependencies checked".to_vec(),
                has_jq,
                prompt_active: false,
            }
        );
    }
}

#[test]
fn prompt_rows_clear_once_ack_twice_and_resume() {
    for (live, fd, expected_out, expected_ack) in [
        (
            true,
            "10",
            b"\r\x1b[K".as_slice(),
            Some(b"ready\n".as_slice()),
        ),
        (false, "10", b"".as_slice(), Some(b"ready\n".as_slice())),
        (true, "", b"\r\x1b[K".as_slice(), None),
        (false, "abc", b"".as_slice(), None),
        (false, "00", b"".as_slice(), Some(b"ready\n".as_slice())),
    ] {
        let mut session = reset(false);
        let (out, next_live, ack) = prompt_pause(&mut session, live, fd);
        assert_eq!(out, expected_out);
        assert!(!next_live);
        assert_eq!(ack.as_deref(), expected_ack);
        assert!(session.prompt_active);
        let (second, _, second_ack) = prompt_pause(&mut session, next_live, fd);
        assert_eq!(second, b"");
        assert_eq!(second_ack.as_deref(), expected_ack);
        prompt_resume(&mut session);
        assert!(!session.prompt_active);
    }
}

#[test]
fn verbose_group_rows_preserve_order_dedup_shapes_and_raw_bytes() {
    let palette = palette();
    let ui = ui(&palette, false);
    let labels = map(&[
        (b"cargo".to_vec(), b"Cargo".to_vec()),
        (b"go".to_vec(), b"Go".to_vec()),
    ]);
    let items = map(&[
        (
            b"cargo".to_vec(),
            b"changed\trg\tfast search\nfailed\tfd\t\n".to_vec(),
        ),
        (b"go".to_vec(), b"ok\tg\t\n".to_vec()),
        (b"c*".to_vec(), b"ok\tstar\td\n".to_vec()),
        (
            b"\xff-group".to_vec(),
            b"changed\t\xff-name\t\xff-detail\n".to_vec(),
        ),
    ]);
    let order = vec![
        b"cargo".to_vec(),
        b"cargo".to_vec(),
        b"go".to_vec(),
        b"c*".to_vec(),
        b"\xff-group".to_vec(),
    ];
    let (cargo, live) =
        print_verbose_group_rows(&ui, true, &order, &items, &labels, &fallback, b"Cargo");
    let mut expected = item(true, b"changed", b"rg", b"fast search");
    expected.extend_from_slice(&item(false, b"failed", b"fd", b""));
    assert_eq!(cargo, expected);
    assert!(!live);
    assert_eq!(
        print_verbose_group_rows(&ui, false, &order, &items, &labels, &fallback, b"c*").0,
        item(false, b"ok", b"star", b"d")
    );
    assert_eq!(
        print_verbose_group_rows(
            &ui,
            false,
            &order,
            &items,
            &labels,
            &fallback,
            b"\xff-group"
        )
        .0,
        item(false, b"changed", b"\xff-name", b"\xff-detail")
    );
    assert_eq!(
        print_verbose_group_rows(&ui, false, &order, &items, &labels, &fallback, b"Other").0,
        b""
    );
}

#[test]
fn verbose_items_merge_shared_labels_and_honor_gates() {
    let palette = palette();
    let labels = map(&[
        (b"cargo".to_vec(), b"Cargo".to_vec()),
        (b"go".to_vec(), b"Go".to_vec()),
    ]);
    let items = map(&[
        (b"cargo".to_vec(), b"changed\trg\tfast\n".to_vec()),
        (b"go".to_vec(), b"ok\tg\t\n".to_vec()),
        (b"github-releases".to_vec(), b"changed\trel\tv2\n".to_vec()),
        (b"github-repos".to_vec(), b"ok\trepo\t\n".to_vec()),
        (b"pip".to_vec(), b"ok\tp\td\n".to_vec()),
    ]);
    let order = vec![b"cargo".to_vec(), b"go".to_vec(), b"pip".to_vec()];
    let (out, live) = print_verbose_items(
        &ui(&palette, false),
        true,
        true,
        &order,
        &items,
        &labels,
        &fallback,
    );
    let mut expected = section(true, b"GitHub");
    expected.extend_from_slice(&item(false, b"changed", b"rel", b"v2"));
    expected.extend_from_slice(&item(false, b"ok", b"repo", b""));
    expected.extend_from_slice(&section(false, b"Cargo"));
    expected.extend_from_slice(&item(false, b"changed", b"rg", b"fast"));
    expected.extend_from_slice(&section(false, b"Go"));
    expected.extend_from_slice(&item(false, b"ok", b"g", b""));
    expected.extend_from_slice(&section(false, b"pip"));
    expected.extend_from_slice(&item(false, b"ok", b"p", b"d"));
    assert_eq!(out, expected);
    assert!(!live);
    assert_eq!(
        print_verbose_items(
            &ui(&palette, false),
            true,
            false,
            &order,
            &items,
            &labels,
            &fallback
        ),
        (Vec::new(), true)
    );
    assert_eq!(
        print_verbose_items(
            &ui(&palette, true),
            false,
            true,
            &order,
            &items,
            &labels,
            &fallback
        )
        .0,
        b""
    );
}

#[test]
fn wanted_status_rows_filter_and_preserve_ifs_shapes() {
    let palette = palette();
    let ui = ui(&palette, false);
    type WantedCase<'a> = (&'a [u8], &'a [u8], &'a [u8], Vec<u8>);
    let cases: &[WantedCase<'_>] = &[
        (
            b"changed\ta\t1\nfailed\tb\t2\nchanged\tc\t\n",
            b"cargo",
            b"changed",
            {
                let mut out = item(false, b"changed", b"a", b"1");
                out.extend_from_slice(&item(false, b"changed", b"c", b""));
                out
            },
        ),
        (
            b"changed\ta\t1\nfailed\tb\t2\nwarning\tc\t3\n",
            b"cargo",
            b"failed",
            item(false, b"failed", b"b", b"2"),
        ),
        (
            b"solo\na\tb\nx\ty\tc\td\n",
            b"cargo",
            b"a",
            item(false, b"a", b"b", b""),
        ),
        (
            b"solo\na\tb\n",
            b"cargo",
            b"solo",
            item(false, b"solo", b"", b""),
        ),
        (
            b"a\t\tb\tc\n",
            b"cargo",
            b"a",
            item(false, b"a", b"b", b"c"),
        ),
        (
            b"\tfoo\tbar\n",
            b"cargo",
            b"foo",
            item(false, b"foo", b"bar", b""),
        ),
        (b"a\tb\t\t\n", b"cargo", b"a", item(false, b"a", b"b", b"")),
    ];
    for (blob, group, wanted, expected) in cases {
        let items = map(&[(group.to_vec(), blob.to_vec())]);
        assert_eq!(
            print_group_items_with_status(&ui, false, &items, group, wanted).0,
            *expected
        );
    }
    let items = map(&[(b"cargo".to_vec(), b"changed\ta\t1\n".to_vec())]);
    assert_eq!(
        print_group_items_with_status(&ui, false, &items, b"go", b"changed").0,
        b""
    );
    assert_eq!(
        print_group_items_with_status(&ui, false, &HashMap::new(), b"cargo", b"changed").0,
        b""
    );
}

#[test]
fn group_summaries_pin_threshold_duration_order_and_failures() {
    let palette = palette();
    let ui = ui(&palette, false);
    let items = map(&[
        (
            b"cargo".to_vec(),
            b"changed\ta\t1\nfailed\tb\t2\nwarning\tc\t3\n".to_vec(),
        ),
        (b"uv".to_vec(), b"warning\tu\told\n".to_vec()),
    ]);

    let summaries = map(&[(
        b"cargo".to_vec(),
        b"changed\tCargo: 1 changed\t900".to_vec(),
    )]);
    let mut changed = note(false, b"changed", b"Cargo: 1 changed");
    changed.extend_from_slice(&item(false, b"changed", b"a", b"1"));
    let mut first_line = note(true, b"failed", b"First, 5ms");
    first_line.extend_from_slice(&item(false, b"failed", b"b", b"2"));
    assert_eq!(
        print_group_summaries(
            &ui,
            false,
            false,
            None,
            &[b"cargo".to_vec()],
            &summaries,
            &items
        )
        .0,
        changed
    );
    assert_eq!(
        print_group_summaries(
            &ui,
            true,
            true,
            None,
            &[b"cargo".to_vec()],
            &summaries,
            &items
        ),
        (Vec::new(), true)
    );

    let summaries = map(&[
        (b"go".to_vec(), b"ok\tGo all good\t10".to_vec()),
        (b"npm".to_vec(), b"skipped\tNPM skipped\t5".to_vec()),
    ]);
    assert_eq!(
        print_group_summaries(
            &ui,
            false,
            false,
            None,
            &[b"go".to_vec(), b"npm".to_vec()],
            &summaries,
            &items
        )
        .0,
        b""
    );

    let summaries = map(&[(b"cargo".to_vec(), b"failed\tCargo: 1 failed\t1500".to_vec())]);
    let mut failed = note(false, b"failed", b"Cargo: 1 failed, 1.5s");
    failed.extend_from_slice(&item(false, b"failed", b"b", b"2"));
    assert_eq!(
        print_group_summaries(
            &ui,
            false,
            false,
            None,
            &[b"cargo".to_vec()],
            &summaries,
            &items
        )
        .0,
        failed
    );

    let summaries = map(&[(b"uv".to_vec(), b"warning\tUV: 1 warning\t42".to_vec())]);
    let mut warning = note(false, b"warning", b"UV: 1 warning, 42ms");
    warning.extend_from_slice(&item(false, b"warning", b"u", b"old"));
    assert_eq!(
        print_group_summaries(
            &ui,
            false,
            false,
            None,
            &[b"uv".to_vec()],
            &summaries,
            &items
        )
        .0,
        warning
    );

    for (raw, rendered) in [
        (b"1500".as_slice(), b"1.5s".as_slice()),
        (b"12000", b"12s"),
        (b"42", b"42ms"),
        (b"010", b"010ms"),
        (b"-5", b"-5ms"),
        (b"abc", b"abcms"),
    ] {
        let summaries = map(&[(b"go".to_vec(), [b"ok\tGo fine\t".as_slice(), raw].concat())]);
        let mut detail = b"Go fine, ".to_vec();
        detail.extend_from_slice(rendered);
        assert_eq!(
            print_group_summaries(
                &ui,
                false,
                false,
                Some(b"0"),
                &[b"go".to_vec()],
                &summaries,
                &items
            )
            .0,
            if matches!(raw, b"010" | b"-5" | b"abc") {
                Vec::new()
            } else {
                note(false, b"ok", &detail)
            }
        );
    }

    let summaries = map(&[(
        b"cargo".to_vec(),
        b"failed\tFirst\t5\nSTALE\tINJECTED\t9".to_vec(),
    )]);
    assert_eq!(
        print_group_summaries(
            &ui,
            true,
            false,
            None,
            &[b"cargo".to_vec(), b"ghost".to_vec(), b"cargo".to_vec()],
            &summaries,
            &items
        )
        .0,
        first_line
    );
}

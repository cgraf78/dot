//! Native byte contracts for Shdeps group labels, summaries, and records.

use dot::shdeps_ui::{State, group_label, summary_text};

#[test]
fn group_labels_are_literal_and_preserve_unknown_bytes() {
    let rows: &[(&[u8], &[u8])] = &[
        (b"packages", b"Packages"),
        (b"github-releases", b"GitHub"),
        (b"github-repos", b"GitHub"),
        (b"cargo", b"Cargo"),
        (b"go", b"Go"),
        (b"uv", b"UV"),
        (b"npm", b"NPM"),
        (b"custom", b"Custom"),
        (b"other", b"Other"),
        (b"", b"Other"),
        (b"pip", b"pip"),
        (b"my-group", b"my-group"),
        (b"Packages", b"Packages"),
        (b"CARGO", b"CARGO"),
        ("grüße".as_bytes(), "grüße".as_bytes()),
        (b"\xff-group", b"\xff-group"),
    ];
    for (group, expected) in rows {
        assert_eq!(group_label(group), *expected, "{group:?}");
    }
}

#[test]
fn summary_text_has_exact_order_plural_quirks_and_fallbacks() {
    for ((changed, current, skipped, failed, warnings), expected) in [
        ((0, 0, 0, 0, 0), b"0 current".as_slice()),
        ((2, 5, 1, 0, 0), b"2 changed, 5 current, 1 skipped"),
        ((0, 0, 0, 3, 0), b"3 failed"),
        ((0, 4, 0, 0, 2), b"2 warning, 4 current"),
        (
            (1, 2, 3, 4, 5),
            b"4 failed, 5 warning, 1 changed, 2 current, 3 skipped",
        ),
        ((0, 7, 0, 1, 0), b"1 failed, 7 current"),
        ((3, 0, 0, 0, 0), b"3 changed"),
        ((0, 0, 2, 0, 0), b"0 current, 2 skipped"),
        ((0, 0, 0, 0, 1), b"1 warning"),
        ((0, 0, 5, 2, 0), b"2 failed, 5 skipped"),
        ((0, -5, 0, 0, 0), b"-5 current"),
        ((0, 0, 0, -2, 0), b"0 current"),
    ] {
        assert_eq!(
            summary_text(changed, current, skipped, failed, warnings),
            expected,
            "({changed}, {current}, {skipped}, {failed}, {warnings})"
        );
    }
}

#[test]
fn state_records_deduplicate_append_overwrite_and_preserve_raw_keys() {
    let mut state = State::new();
    state.remember_group(b"cargo");
    state.remember_group(b"cargo");
    state.remember_group(b"go");
    assert_eq!(state.order(), [b"cargo".to_vec(), b"go".to_vec()]);

    let mut state = State::new();
    state.record_item(b"cargo", b"changed", b"ripgrep", b"fast search");
    state.record_item(b"cargo", b"failed", b"", b"");
    assert_eq!(state.order(), [b"cargo".to_vec()]);
    assert_eq!(
        state.items_blob(b"cargo"),
        Some(b"changed\tripgrep\tfast search\nfailed\t\t\n".as_slice())
    );
    assert_eq!(state.summary_blob(b"cargo"), None);
    assert_eq!(state.display_label(b"cargo"), b"Cargo");

    let mut state = State::new();
    state.remember_group(b"");
    state.record_item(b"", b"ok", b"mystery", b"no group");
    state.record_group_summary(b"", b"", b"ok", 0, 3, 0, 0, b"10", 0);
    assert!(state.order().is_empty());
    assert_eq!(state.items_blob(b""), None);
    assert_eq!(state.summary_blob(b""), None);
    assert_eq!(state.display_label(b""), b"Other");

    let mut state = State::new();
    state.record_group_summary(b"cargo", b"", b"changed", 1, 2, 0, 0, b"1500", 0);
    assert_eq!(state.display_label(b"cargo"), b"Cargo");
    assert_eq!(
        state.summary_blob(b"cargo"),
        Some(b"changed\tCargo: 1 changed, 2 current\t1500".as_slice())
    );

    let mut state = State::new();
    state.record_group_summary(b"pip", b"Pip Extra", b"ok", 0, 9, 0, 0, b"", 0);
    state.record_group_summary(b"brew", b"", b"ok", 0, 3, 0, 0, b"200", 0);
    assert_eq!(state.order(), [b"pip".to_vec(), b"brew".to_vec()]);
    assert_eq!(state.display_label(b"pip"), b"Pip Extra");
    assert_eq!(
        state.summary_blob(b"pip"),
        Some(b"ok\tPip Extra: 9 current\t0".as_slice())
    );
    assert_eq!(state.display_label(b"brew"), b"brew");
    assert_eq!(
        state.summary_blob(b"brew"),
        Some(b"ok\tbrew: 3 current\t200".as_slice())
    );

    let mut state = State::new();
    state.record_group_summary(b"uv", b"", b"warning", 0, 1, 0, 0, b"42", 0);
    assert_eq!(
        state.summary_blob(b"uv"),
        Some(b"warning\tUV: 1 current\t42".as_slice())
    );

    let mut state = State::new();
    state.record_item(b"npm", b"changed", b"left-pad", b"new api");
    state.record_group_summary(b"npm", b"", b"changed", 1, 4, 0, 0, b"900", 0);
    state.record_item(b"npm", b"failed", b"right-pad", b"rate limited");
    state.record_group_summary(b"npm", b"NPM", b"failed", 1, 4, 0, 1, b"950", 0);
    assert_eq!(state.order(), [b"npm".to_vec()]);
    assert_eq!(
        state.items_blob(b"npm"),
        Some(b"changed\tleft-pad\tnew api\nfailed\tright-pad\trate limited\n".as_slice())
    );
    assert_eq!(
        state.summary_blob(b"npm"),
        Some(b"failed\tNPM: 1 failed, 1 changed, 4 current\t950".as_slice())
    );
    assert_eq!(state.display_label(b"other"), b"Other");

    let mut state = State::new();
    state.remember_group(b"\xff-group");
    state.record_item(b"\xff-group", b"ok", b"n", b"d");
    assert_eq!(state.order(), [b"\xff-group".to_vec()]);
    assert_eq!(
        state.items_blob(b"\xff-group"),
        Some(b"ok\tn\td\n".as_slice())
    );
    assert_eq!(state.display_label(b"\xff-group"), b"\xff-group");
}

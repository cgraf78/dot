//! Native contracts for doctor extension coordination.

use dot::doctor_coordinator::{
    SpecError, SummaryColor, collect_specs, collect_specs_with, extension_identity, extension_key,
    overall_ok, record_kind, source_relative_valid, summary_color, summary_line,
};
use dot::doctor_runtime::Kind;
use dot_test_support::TempDir;

#[test]
fn identity_rows_agree() {
    for (key, identity) in [
        (&b"foo"[..], Some(&b"foo"[..])),
        (b"20-foo", Some(&b"foo"[..])),
        (b"20_foo-2", Some(&b"foo-2"[..])),
        (b"Foo", None),
        (b"1a", None),
        (b"12_3a", None),
        (b"", None),
    ] {
        assert_eq!(extension_identity(key), identity, "{key:?}");
    }
    assert_eq!(extension_key(b"/x/20-foo.sh"), b"20-foo");
    assert_eq!(extension_key(b"a.sh.sh"), b"a.sh");
}

#[test]
fn specs_rows_agree() {
    let dir = TempDir::new("doctor-specs-native").expect("fixture");
    for name in ["20-zed.sh", "10-alpha.sh", ".hidden.sh", "ignored"] {
        dir.write(name, b"#!/bin/sh\n");
    }
    let discovery = collect_specs(dir.path()).expect("collect");
    assert!(discovery.error.is_none());
    assert_eq!(
        discovery
            .specs
            .iter()
            .map(|s| s.key.as_slice())
            .collect::<Vec<_>>(),
        vec![b"10-alpha".as_slice(), b"20-zed".as_slice()]
    );

    dir.write("15-Bad.sh", b"");
    let invalid = collect_specs(dir.path()).expect("collect invalid");
    assert!(matches!(
        invalid.error,
        Some(SpecError::InvalidIdentity { .. })
    ));

    let duplicate = TempDir::new("doctor-specs-duplicate").expect("fixture");
    duplicate.write("10-same.sh", b"");
    duplicate.write("same.sh", b"");
    assert!(matches!(
        collect_specs(duplicate.path()).expect("duplicate").error,
        Some(SpecError::DuplicateIdentity { .. })
    ));

    let unsafe_discovery = collect_specs_with(duplicate.path(), |_| false).expect("unsafe");
    assert!(matches!(
        unsafe_discovery.error,
        Some(SpecError::Unsafe { .. })
    ));
}

#[test]
fn dispatch_rows_agree() {
    for (raw, expected) in [
        (&b"section"[..], Kind::Section),
        (&b"ok"[..], Kind::Ok),
        (&b"warn"[..], Kind::Warn),
        (&b"fail"[..], Kind::Fail),
        (&b"skip"[..], Kind::Skip),
        (&b"future"[..], Kind::Unknown),
        (&b""[..], Kind::Unknown),
    ] {
        assert_eq!(record_kind(raw), expected, "{raw:?}");
    }
}

#[test]
fn source_rows_agree() {
    for valid in ["doctor.d/a.sh", "doctor.d/nested/a.sh", "a.sh"] {
        assert!(source_relative_valid(valid.as_bytes()), "{valid}");
    }
    for invalid in ["", "/absolute", "../escape", "a/../escape", "a//b", "a/./b"] {
        assert!(!source_relative_valid(invalid.as_bytes()), "{invalid}");
    }
}

#[test]
fn summary_rows_agree() {
    assert_eq!(summary_line(1, 2, 3), "1 passed · 2 warnings · 3 failed");
    assert_eq!(summary_color(1, 0), SummaryColor::Red);
    assert_eq!(summary_color(0, 1), SummaryColor::Yellow);
    assert_eq!(summary_color(0, 0), SummaryColor::Green);
    assert!(overall_ok(0, 0));
    assert!(!overall_ok(1, 0));
    assert!(!overall_ok(0, 1));
}

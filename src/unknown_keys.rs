//! Forward-compatible keys in Dot's strict `key=value` data files.
//!
//! The client config, profile definitions, profile selectors, and overlay
//! descriptors travel through client and overlay repositories, which update
//! independently of the Dot release reading them. Rejecting a well-formed key
//! this release does not know would let one newer repository commit stop every
//! lagging host, including the update that would upgrade it. Each parser
//! therefore keeps every rule it knows strict and treats a key it does not know
//! by one of two outcomes:
//!
//! - a near miss of a key the file knows (at most [`TYPO_EDITS`] edits) is
//!   almost certainly a typo, because new keys are kept further than that from
//!   existing ones (each file's known-key test enforces it);
//! - anything else most likely comes from a newer Dot, and the file decides
//!   what is safe to do without it ([`Effect`]).
//!
//! The parsers never print. They record a [`DataKey`] per distinct key and file
//! for the command boundary (stderr) or `dot doctor` (a warning row) to report.

/// Most edits between an unknown key and a known one for the unknown key to
/// read as a misspelling of the known one. Two cover a swapped or dropped
/// letter without matching unrelated keys.
pub const TYPO_EDITS: usize = 2;

/// The key in `known` that `key` most plausibly misspells, if any. A rare tie
/// picks the first in `known` order.
pub fn suggestion(key: &str, known: &[&'static str]) -> Option<&'static str> {
    known
        .iter()
        .map(|candidate| (edit_distance(key, candidate), *candidate))
        .filter(|(distance, _)| *distance <= TYPO_EDITS)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate)
}

/// The first pair of `known` keys within [`TYPO_EDITS`] of each other.
///
/// A release that adds such a key would make older releases read it as a typo
/// of the other (a hard error, or a degraded update for the client config), so
/// each file's tests assert this returns `None` for its known keys.
pub fn crowded_pair(known: &[&'static str]) -> Option<(&'static str, &'static str)> {
    known.iter().enumerate().find_map(|(index, left)| {
        known[index + 1..]
            .iter()
            .find(|right| edit_distance(left, right) <= TYPO_EDITS)
            .map(|right| (*left, *right))
    })
}

/// Levenshtein distance over bytes (keys are ASCII by construction).
pub(crate) fn edit_distance(left: &str, right: &str) -> usize {
    let right = right.as_bytes();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for (i, a) in left.bytes().enumerate() {
        let mut current = vec![i + 1; right.len() + 1];
        for (j, b) in right.iter().enumerate() {
            let substitute = previous[j] + usize::from(a != *b);
            current[j + 1] = substitute.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        previous = current;
    }
    previous[right.len()]
}

/// What a release does with a data file holding a key it does not know.
///
/// The choice follows what missing the key can cost. A profile definition only
/// adds members, so ignoring a key can at worst select fewer overlays. A
/// selector is a predicate and a descriptor decides what is cloned and linked:
/// an unknown key there is most likely one more condition (a match restriction,
/// a pinned revision, a trust rule), and ignoring it could match more hosts or
/// activate an overlay the newer Dot would not. Those fail closed instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// The key is ignored and the rest of the file applies.
    Ignored,
    /// The selector never matches, and it could not have changed the choice
    /// (another selector is more specific, or its user or host differs).
    SelectorSkipped,
    /// The selector never matches, and it could have chosen this host's
    /// profile, so selection fell back to `base` rather than to whatever
    /// less specific selector or default would otherwise apply.
    SelectorFallback,
    /// The named `base` overlay was skipped, so its personal selectors,
    /// which could outrank every selector read, went unread and selection
    /// fell back to `base`.
    SelectorsUnread(String),
    /// The named overlay is selected but never activated.
    OverlaySkipped(String),
}

impl Effect {
    /// Whether a key with this effect leaves this release unsure which
    /// overlays the newer Dot would activate: a skipped descriptor, or a
    /// selection that fell back because a skipped selector (or the unread
    /// selectors of a skipped overlay) could have chosen differently.
    ///
    /// `dot update` then holds the installed overlay set instead of
    /// converging to this release's partial reading (see
    /// `update_engine`). An ignored definition key does not hold: the key
    /// rules keep definition keys additive, so missing one can only leave
    /// out overlays that are not active yet. A skipped selector that could
    /// not have won changes nothing.
    pub fn holds(&self) -> bool {
        matches!(
            self,
            Effect::SelectorFallback | Effect::SelectorsUnread(_) | Effect::OverlaySkipped(_)
        )
    }

    /// Whether this key came from a selector (selector keys are forgotten
    /// with the selector records they belong to).
    pub fn is_selector(&self) -> bool {
        matches!(
            self,
            Effect::SelectorSkipped | Effect::SelectorFallback | Effect::SelectorsUnread(_)
        )
    }
}

/// A well-formed key a data file holds that this release does not know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataKey {
    /// File holding the key, as discovery spelled it.
    pub path: String,
    /// One-based line of the key's first occurrence in that file.
    pub line: usize,
    /// The key exactly as written (`[a-z_]+`, so safe to print).
    pub key: String,
    /// What this release did about it.
    pub effect: Effect,
}

impl DataKey {
    /// Diagnostic domain prefix, matching the file's error messages.
    fn domain(&self) -> &'static str {
        match self.effect {
            Effect::Ignored
            | Effect::SelectorSkipped
            | Effect::SelectorFallback
            | Effect::SelectorsUnread(_) => "profile",
            Effect::OverlaySkipped(_) => "overlay",
        }
    }

    /// What happened, after the key (`ignored`, `selector skipped`, ...).
    pub fn outcome(&self) -> String {
        match &self.effect {
            Effect::Ignored => "ignored".to_string(),
            Effect::SelectorSkipped => "selector skipped".to_string(),
            Effect::SelectorFallback => "selector skipped, profile 'base' selected".to_string(),
            Effect::SelectorsUnread(name) => {
                format!("overlay '{name}' selectors unread, profile 'base' selected")
            }
            Effect::OverlaySkipped(name) => format!("overlay '{name}' skipped"),
        }
    }

    /// The stable one-line stderr warning (no trailing newline).
    pub fn warning(&self) -> String {
        let separator = if self.effect == Effect::Ignored {
            " "
        } else {
            "; "
        };
        format!(
            "dot: {}: warning: {}: unknown key '{}'{separator}{} (newer dot?)",
            self.domain(),
            self.path,
            self.key,
            self.outcome()
        )
    }
}

/// Record `key` for `path` unless the same file already recorded it, so a
/// repeated key, or a file a run reads more than once, reports once.
pub fn record(keys: &mut Vec<DataKey>, entry: DataKey) {
    if !keys
        .iter()
        .any(|seen| seen.path == entry.path && seen.key == entry.key)
    {
        keys.push(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(effect: Effect) -> DataKey {
        DataKey {
            path: "/c/dot/f.conf".to_string(),
            line: 3,
            key: "future_key".to_string(),
            effect,
        }
    }

    #[test]
    fn suggestion_needs_a_near_miss() {
        let known = ["version", "profiles", "overlays"];
        assert_eq!(suggestion("overlay", &known), Some("overlays"));
        assert_eq!(suggestion("profles", &known), Some("profiles"));
        assert_eq!(suggestion("versionab", &known), Some("version"));
        assert_eq!(suggestion("versionabc", &known), None);
        assert_eq!(suggestion("future_key", &known), None);
    }

    #[test]
    fn crowded_pair_finds_keys_within_two_edits() {
        assert_eq!(
            crowded_pair(&["user", "users", "host"]),
            Some(("user", "users"))
        );
        assert_eq!(crowded_pair(&["user", "host", "profile"]), None);
    }

    #[test]
    fn warnings_name_the_file_key_and_effect() {
        assert_eq!(
            key(Effect::Ignored).warning(),
            "dot: profile: warning: /c/dot/f.conf: unknown key 'future_key' ignored (newer dot?)"
        );
        assert_eq!(
            key(Effect::SelectorSkipped).warning(),
            "dot: profile: warning: /c/dot/f.conf: unknown key 'future_key'; selector skipped (newer dot?)"
        );
        assert_eq!(
            key(Effect::SelectorFallback).warning(),
            "dot: profile: warning: /c/dot/f.conf: unknown key 'future_key'; selector skipped, profile 'base' selected (newer dot?)"
        );
        assert_eq!(
            key(Effect::SelectorsUnread("personal".to_string())).warning(),
            "dot: profile: warning: /c/dot/f.conf: unknown key 'future_key'; overlay 'personal' selectors unread, profile 'base' selected (newer dot?)"
        );
        assert_eq!(
            key(Effect::OverlaySkipped("beta".to_string())).warning(),
            "dot: overlay: warning: /c/dot/f.conf: unknown key 'future_key'; overlay 'beta' skipped (newer dot?)"
        );
    }

    #[test]
    fn only_effects_that_change_the_overlay_set_hold_it() {
        assert!(!Effect::Ignored.holds());
        assert!(!Effect::SelectorSkipped.holds());
        assert!(Effect::SelectorFallback.holds());
        assert!(Effect::SelectorsUnread("p".to_string()).holds());
        assert!(Effect::OverlaySkipped("p".to_string()).holds());
    }

    #[test]
    fn record_keeps_one_entry_per_file_and_key() {
        let mut keys = Vec::new();
        record(&mut keys, key(Effect::Ignored));
        record(
            &mut keys,
            DataKey {
                line: 9,
                ..key(Effect::Ignored)
            },
        );
        record(
            &mut keys,
            DataKey {
                path: "/c/dot/g.conf".to_string(),
                ..key(Effect::Ignored)
            },
        );
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].line, 3);
    }
}

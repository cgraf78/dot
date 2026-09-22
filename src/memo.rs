//! Process-lifetime memoization for immutable probes.
//!
//! Several engine probes (`uname -s`, short hostname, CPU count) shell
//! out once per call site but answer from state that cannot change
//! mid-process, so `dot update` pays for each several times per run.
//! [`Memo`] caches the first definitive answer; callers pass only
//! definitive results (`None` from a failed spawn re-probes, so a
//! transient failure never pins). Unlike the overlay repository
//! cache ([`crate::overlays`] invalidation on clone/hooks), these
//! answers have no engine mutation point and need no invalidation.

use std::sync::Mutex;

/// A single memoized probe answer. Thread-safe: concurrent first
/// fills may probe redundantly, but every fill stores the same
/// definitive answer.
#[derive(Debug, Default)]
pub(crate) struct Memo<T: Clone> {
    slot: Mutex<Option<T>>,
}

impl<T: Clone> Memo<T> {
    pub(crate) const fn new() -> Self {
        Self {
            slot: Mutex::new(None),
        }
    }

    /// Return the memoized answer, or `None` when nothing is cached
    /// yet (or the lock is poisoned, which falls back to probing).
    pub(crate) fn get(&self) -> Option<T> {
        self.slot.lock().ok().and_then(|slot| slot.clone())
    }

    /// Store a definitive answer. A poisoned lock keeps the old
    /// answer (callers re-probe instead of observing garbage).
    pub(crate) fn set(&self, value: T) {
        if let Ok(mut slot) = self.slot.lock() {
            *slot = Some(value);
        }
    }

    /// Return the memoized answer, probing once on a miss. A `None`
    /// probe result is definitive-for-nothing and stays uncached.
    pub(crate) fn get_or_probe(&self, probe: impl FnOnce() -> Option<T>) -> Option<T> {
        if let Some(cached) = self.get() {
            return Some(cached);
        }
        let probed = probe()?;
        self.set(probed.clone());
        Some(probed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn memo_probes_once_then_serves_cached_answers() {
        let memo = Memo::new();
        let probes = AtomicUsize::new(0);
        let probe = || {
            probes.fetch_add(1, Ordering::SeqCst);
            Some("linux".to_string())
        };
        assert_eq!(memo.get_or_probe(probe), Some("linux".to_string()));
        assert_eq!(
            memo.get_or_probe(|| {
                probes.fetch_add(1, Ordering::SeqCst);
                Some("changed".to_string())
            }),
            Some("linux".to_string())
        );
        assert_eq!(probes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failed_probes_stay_uncached() {
        let memo: Memo<String> = Memo::new();
        let probes = AtomicUsize::new(0);
        assert_eq!(
            memo.get_or_probe(|| {
                probes.fetch_add(1, Ordering::SeqCst);
                None
            }),
            None
        );
        assert_eq!(
            memo.get_or_probe(|| {
                probes.fetch_add(1, Ordering::SeqCst);
                Some("recovered".to_string())
            }),
            Some("recovered".to_string())
        );
        assert_eq!(probes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn concurrent_fills_agree_on_one_answer() {
        let memo = Arc::new(Memo::new());
        let probes = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let memo = Arc::clone(&memo);
                let probes = Arc::clone(&probes);
                scope.spawn(move || {
                    let answer = memo.get_or_probe(|| {
                        probes.fetch_add(1, Ordering::SeqCst);
                        Some("linux".to_string())
                    });
                    assert_eq!(answer, Some("linux".to_string()));
                });
            }
        });
        assert_eq!(memo.get(), Some("linux".to_string()));
        assert!(probes.load(Ordering::SeqCst) >= 1);
    }
}

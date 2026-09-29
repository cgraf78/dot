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
//!
//! [`cached_probe`] is the keyed variant for probes whose answers
//! the engine can mutate (checkout revisions, upstream names):
//! same Some-only caching plus a cancellation gate, with explicit
//! invalidation at the mutation boundaries. Callers pass the
//! cancellation verdict they already read so unit tests can drive
//! both branches without touching the process signal latch.

use std::collections::HashMap;
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

/// Memoized keyed probe with a cancellation gate. When `cancelled`,
/// return missing without consulting or filling the cache: every
/// caller routes through supervised spawns that observe the latched
/// signal and fail, so serving a stale hit would let teardown
/// proceed as if uninterrupted. Only `Some` answers pin; `None`
/// re-probes so transient failures never stick. A poisoned lock
/// degrades to probing (miss plus skipped store).
pub(crate) fn cached_probe(
    cache: &Mutex<HashMap<Vec<u8>, String>>,
    key: &[u8],
    cancelled: bool,
    probe: impl FnOnce() -> Option<String>,
) -> Option<String> {
    if cancelled {
        return None;
    }
    if let Ok(cache) = cache.lock() {
        if let Some(hit) = cache.get(key) {
            return Some(hit.clone());
        }
    }
    let answer = probe()?;
    if let Ok(mut cache) = cache.lock() {
        cache.insert(key.to_vec(), answer.clone());
    }
    Some(answer)
}

/// Test-only exclusion for the engine's process-global probe caches.
///
/// Unit tests that count `git` spawns need the caches to hold still, but
/// two process-wide effects legitimately reach them from other test
/// threads through production paths:
///
/// - global clears ([`crate::overlays::invalidate_worktree_cache`],
///   [`crate::repos_config::invalidate_config_cache`],
///   [`crate::repos_base::invalidate_client_match_cache`]) run after every
///   hook worker, git passthrough, provider run, and staged clone; and
/// - a latched handled signal makes every cached probe bypass its cache
///   (serving a stale hit during teardown would be wrong), and signal
///   tests latch the process-wide flag while they own the handlers.
///
/// Either effect between a counting test's two probes re-spawns `git`
/// and doubles its count. Rather than making every such test take a lock,
/// each global-clear chokepoint takes a shared guard, and counting tests
/// hold the exclusive side plus signal-handler ownership, which every
/// signal test holds for as long as its handlers can latch. The holding
/// thread may still clear the caches itself: a thread-local flag skips the
/// shared guard so it cannot deadlock on its own exclusive lock.
///
/// Lock order is signal ownership, then the gate: a signal test can reach
/// a global clear (the shared side) while owning the handlers, so taking
/// the gate first could deadlock against it.
#[cfg(test)]
pub(crate) mod probe_cache_test_gate {
    use std::cell::Cell;
    use std::collections::HashSet;
    use std::sync::{Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
    use std::thread::ThreadId;
    use std::time::{Duration, Instant};

    static GATE: RwLock<()> = RwLock::new(());

    /// Threads that have reached a gate acquisition (shared or exclusive).
    /// Lets the gate's own tests prove a worker is at the lock before
    /// asserting it is held there, instead of inferring it from elapsed
    /// time. Thread IDs are never reused, so entries never go stale.
    static ARRIVALS: Mutex<Option<HashSet<ThreadId>>> = Mutex::new(None);

    fn note_arrival() {
        let mut arrivals = ARRIVALS.lock().unwrap_or_else(PoisonError::into_inner);
        arrivals
            .get_or_insert_with(HashSet::new)
            .insert(std::thread::current().id());
    }

    /// Wait (bounded) until `thread` has reached a gate acquisition.
    pub(crate) fn wait_for_arrival(thread: ThreadId, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            let arrived = ARRIVALS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .is_some_and(|arrivals| arrivals.contains(&thread));
            if arrived {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    thread_local! {
        static EXCLUSIVE: Cell<bool> = const { Cell::new(false) };
    }

    /// Holds the probe caches still for the current test until dropped.
    pub(crate) struct Exclusive {
        // Field order is drop order: release the gate before handler
        // ownership, the reverse of acquisition.
        _guard: RwLockWriteGuard<'static, ()>,
        _signals: MutexGuard<'static, ()>,
    }

    impl Drop for Exclusive {
        fn drop(&mut self) {
            EXCLUSIVE.with(|flag| flag.set(false));
        }
    }

    /// Block every other thread's global clear and signal latch until the
    /// guard drops.
    pub(crate) fn exclusive() -> Exclusive {
        note_arrival();
        let signals = crate::cleanup::hold_signal_ownership_for_test();
        let guard = GATE.write().unwrap_or_else(PoisonError::into_inner);
        EXCLUSIVE.with(|flag| flag.set(true));
        Exclusive {
            _guard: guard,
            _signals: signals,
        }
    }

    /// The guard a global clear takes; `None` on the exclusive holder's
    /// own thread.
    pub(crate) fn shared() -> Option<RwLockReadGuard<'static, ()>> {
        if EXCLUSIVE.with(Cell::get) {
            return None;
        }
        note_arrival();
        Some(GATE.read().unwrap_or_else(PoisonError::into_inner))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Run `op` on another thread while `held` keeps the probe caches
    /// still, and prove it waits at the gate: the worker must first be
    /// observed arriving at a gate acquisition (so it cannot slip past
    /// after release), then must not finish while the gate is held. The
    /// "not finished" observation is one-directional and can never fail
    /// spuriously under load; arrival and resumption get generous bounded
    /// deadlines.
    fn held_at_gate_until_release(
        held: probe_cache_test_gate::Exclusive,
        op: impl FnOnce() + Send + 'static,
    ) {
        let (done, finished) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            op();
            let _ = done.send(());
        });
        assert!(
            probe_cache_test_gate::wait_for_arrival(worker.thread().id(), Duration::from_secs(30)),
            "operation never reached the probe cache gate"
        );
        assert!(
            finished.recv_timeout(Duration::from_millis(100)).is_err(),
            "gated operation completed while the probe caches were held"
        );
        drop(held);
        finished
            .recv_timeout(Duration::from_secs(30))
            .expect("gated operation did not resume after release");
        worker.join().expect("gated worker panicked");
    }

    #[test]
    fn probe_cache_gate_holds_foreign_global_clears_until_released() {
        held_at_gate_until_release(
            probe_cache_test_gate::exclusive(),
            crate::repos_config::invalidate_config_cache,
        );
        held_at_gate_until_release(
            probe_cache_test_gate::exclusive(),
            crate::repos_base::invalidate_client_match_cache,
        );
        held_at_gate_until_release(
            probe_cache_test_gate::exclusive(),
            crate::overlays::invalidate_worktree_cache,
        );
    }

    #[test]
    fn probe_cache_gate_holds_signal_ownership() {
        // Deterministic: while the exclusive side is held, no other thread
        // can own the handlers, so none can latch the process-wide signal.
        let _held = probe_cache_test_gate::exclusive();
        let held_elsewhere = std::thread::spawn(crate::cleanup::signal_ownership_is_held_for_test)
            .join()
            .expect("probe thread");
        assert!(
            held_elsewhere,
            "exclusive probe cache gate left signal ownership free"
        );
    }

    #[test]
    fn probe_cache_gate_orders_signal_ownership_before_the_gate() {
        // A signal test can reach a global clear while owning the handlers.
        // `exclusive()` must therefore wait for handler ownership before
        // taking the gate; the reverse order would deadlock here (the
        // counting thread holding the gate, the signal owner waiting on it).
        let (owner_ready, owner_is_ready) = std::sync::mpsc::channel();
        let (go, may_clear) = std::sync::mpsc::channel::<()>();
        let (cleared, owner_cleared) = std::sync::mpsc::channel();
        let owner = std::thread::spawn(move || {
            let _signals = crate::cleanup::hold_signal_ownership_for_test();
            owner_ready.send(()).expect("owner ready");
            may_clear.recv().expect("clear order");
            crate::repos_config::invalidate_config_cache();
            let _ = cleared.send(());
        });
        owner_is_ready
            .recv_timeout(Duration::from_secs(30))
            .expect("signal owner did not start");
        let (counted, counting_done) = std::sync::mpsc::channel();
        let counter = std::thread::spawn(move || {
            let _held = probe_cache_test_gate::exclusive();
            let _ = counted.send(());
        });
        assert!(
            probe_cache_test_gate::wait_for_arrival(counter.thread().id(), Duration::from_secs(30)),
            "counting thread never reached the gate"
        );
        go.send(()).expect("release the signal owner");
        // Bounded: under the wrong lock order this clear deadlocks, and the
        // test must fail rather than hang.
        owner_cleared
            .recv_timeout(Duration::from_secs(30))
            .expect("signal owner's clear deadlocked against the counting thread");
        owner.join().expect("signal owner");
        counting_done
            .recv_timeout(Duration::from_secs(30))
            .expect("counting thread did not acquire the gate after the owner");
        counter.join().expect("counting thread");
    }

    #[test]
    fn probe_cache_gate_holder_may_clear_on_its_own_thread() {
        // A counting test's own code path (e.g. a config repair) clears
        // the caches; that must not deadlock on the holder's write lock.
        let _held = probe_cache_test_gate::exclusive();
        crate::repos_config::invalidate_config_cache();
        crate::repos_base::invalidate_client_match_cache();
        crate::overlays::invalidate_worktree_cache();
    }

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

    fn keyed_probes(
        cache: &Mutex<HashMap<Vec<u8>, String>>,
        key: &[u8],
        cancelled: bool,
        calls: &std::cell::Cell<usize>,
        answer: Option<&str>,
    ) -> Option<String> {
        cached_probe(cache, key, cancelled, || {
            calls.set(calls.get() + 1);
            answer.map(str::to_string)
        })
    }

    #[test]
    fn keyed_probe_memoizes_one_key_across_reads() {
        let cache = Mutex::new(HashMap::new());
        let calls = std::cell::Cell::new(0);
        for _ in 0..3 {
            assert_eq!(
                keyed_probes(&cache, b"/repo/root", false, &calls, Some("abc123")),
                Some("abc123".to_string())
            );
        }
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn keyed_probe_keys_answers_separately() {
        let cache = Mutex::new(HashMap::new());
        let calls = std::cell::Cell::new(0);
        assert_eq!(
            keyed_probes(&cache, b"/repo/a", false, &calls, Some("aaa")),
            Some("aaa".to_string())
        );
        assert_eq!(
            keyed_probes(&cache, b"/repo/b", false, &calls, Some("bbb")),
            Some("bbb".to_string())
        );
        assert_eq!(
            keyed_probes(&cache, b"/repo/a", false, &calls, Some("aaa")),
            Some("aaa".to_string())
        );
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn keyed_probe_reprobes_after_invalidate() {
        let cache = Mutex::new(HashMap::new());
        let calls = std::cell::Cell::new(0);
        assert_eq!(
            keyed_probes(&cache, b"/repo/root", false, &calls, Some("before")),
            Some("before".to_string())
        );
        cache.lock().unwrap().clear();
        assert_eq!(
            keyed_probes(&cache, b"/repo/root", false, &calls, Some("after")),
            Some("after".to_string())
        );
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn keyed_probe_failures_stay_uncached() {
        let cache = Mutex::new(HashMap::new());
        let calls = std::cell::Cell::new(0);
        assert_eq!(
            keyed_probes(&cache, b"/repo/root", false, &calls, None),
            None
        );
        assert_eq!(
            keyed_probes(&cache, b"/repo/root", false, &calls, Some("recovered")),
            Some("recovered".to_string())
        );
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn keyed_probe_returns_missing_when_cancelled_without_probing() {
        let cache = Mutex::new(HashMap::new());
        let calls = std::cell::Cell::new(0);
        assert_eq!(
            keyed_probes(&cache, b"/repo/root", false, &calls, Some("abc123")),
            Some("abc123".to_string())
        );
        assert_eq!(
            keyed_probes(&cache, b"/repo/root", true, &calls, Some("abc123")),
            None
        );
        assert_eq!(calls.get(), 1);
    }
}

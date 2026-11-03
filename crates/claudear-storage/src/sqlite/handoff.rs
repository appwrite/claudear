use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LockResult, Mutex, MutexGuard};

/// Counts lock requests and grants so a thread that relocks in a loop can hand an unfair
/// `std::sync::Mutex` to the threads already waiting on it instead of starving them.
#[derive(Debug, Default)]
pub(crate) struct Handoff {
    requested: AtomicU64,
    granted: AtomicU64,
}

impl Handoff {
    pub(crate) fn lock<'a, T>(&self, mutex: &'a Mutex<T>) -> LockResult<MutexGuard<'a, T>> {
        self.requested.fetch_add(1, Ordering::SeqCst);
        let guard = mutex.lock();
        self.granted.fetch_add(1, Ordering::SeqCst);
        guard
    }

    /// Wait until a thread that was already waiting for the lock has acquired it. The caller
    /// must not hold the lock, or this deadlocks whenever another thread is waiting.
    pub(crate) fn yield_to_waiters(&self) {
        let granted = self.granted.load(Ordering::SeqCst);
        if self.requested.load(Ordering::SeqCst) == granted {
            return;
        }
        while self.granted.load(Ordering::SeqCst) == granted {
            std::thread::yield_now();
        }
    }

    #[cfg(test)]
    pub(crate) fn granted(&self) -> u64 {
        self.granted.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_yield_to_waiters_returns_immediately_without_waiters() {
        let handoff = Handoff::default();
        let mutex = Mutex::new(());
        drop(handoff.lock(&mutex).unwrap());

        handoff.yield_to_waiters();
    }

    #[test]
    fn test_yield_to_waiters_lets_a_queued_waiter_take_the_lock_first() {
        let handoff = Arc::new(Handoff::default());
        let mutex = Arc::new(Mutex::new(false));
        let guard = handoff.lock(&mutex).unwrap();

        let waiter = {
            let handoff = Arc::clone(&handoff);
            let mutex = Arc::clone(&mutex);
            std::thread::spawn(move || *handoff.lock(&mutex).unwrap() = true)
        };
        while handoff.requested.load(Ordering::SeqCst) == handoff.granted.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        drop(guard);
        handoff.yield_to_waiters();

        assert!(
            *handoff.lock(&mutex).unwrap(),
            "relocked before the queued waiter acquired the lock"
        );
        waiter.join().unwrap();
    }
}

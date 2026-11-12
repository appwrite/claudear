use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::Duration;

/// Lock order: the inner mutex is a leaf; nothing else is locked while it is held.
#[derive(Debug, Default)]
pub(crate) struct Signal {
    raised: Mutex<bool>,
    condvar: Condvar,
    stopped: AtomicBool,
}

impl Signal {
    pub(crate) fn raise(&self) {
        *self.raised.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.condvar.notify_all();
    }

    pub(crate) fn stop(&self) {
        let _raised = self.raised.lock().unwrap_or_else(PoisonError::into_inner);
        self.stopped.store(true, Ordering::Release);
        self.condvar.notify_all();
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn is_raised(&self) -> bool {
        *self.raised.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Blocks until the signal is stopped or `duration` elapses, leaving a raise for the next
    /// [`Self::wait`].
    pub(crate) fn rest(&self, duration: Duration) {
        let raised = self.raised.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = self
            .condvar
            .wait_timeout_while(raised, duration, |_| !self.is_stopped())
            .unwrap_or_else(PoisonError::into_inner);
    }

    /// Blocks until the signal is raised or stopped, or `timeout` elapses, and clears a raise.
    pub(crate) fn wait(&self, timeout: Duration) {
        let raised = self.raised.lock().unwrap_or_else(PoisonError::into_inner);
        let (mut raised, _) = self
            .condvar
            .wait_timeout_while(raised, timeout, |raised| !*raised && !self.is_stopped())
            .unwrap_or_else(PoisonError::into_inner);
        *raised = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc};
    use std::time::Instant;

    const PROMPT: Duration = Duration::from_secs(5);
    const LONG: Duration = Duration::from_secs(600);

    fn waiting(signal: &Arc<Signal>) -> mpsc::Receiver<()> {
        let (sender, receiver) = mpsc::channel();
        let signal = Arc::clone(signal);
        std::thread::spawn(move || {
            signal.wait(LONG);
            let _ = sender.send(());
        });
        receiver
    }

    #[test]
    fn test_signal_raised_before_waiting_returns_immediately_and_clears() {
        let signal = Signal::default();
        signal.raise();
        let started = Instant::now();
        signal.wait(LONG);
        assert!(started.elapsed() < PROMPT);

        let started = Instant::now();
        signal.wait(Duration::from_millis(50));
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "a raise must be consumed by the wait that observed it"
        );
    }

    #[test]
    fn test_signal_raise_wakes_a_waiter() {
        let signal = Arc::new(Signal::default());
        let woken = waiting(&signal);
        std::thread::sleep(Duration::from_millis(20));
        signal.raise();
        woken
            .recv_timeout(PROMPT)
            .expect("raise must wake a waiting thread");
    }

    #[test]
    fn test_signal_rest_lasts_its_duration_and_keeps_a_raise() {
        let signal = Arc::new(Signal::default());
        let resting = {
            let signal = Arc::clone(&signal);
            std::thread::spawn(move || {
                let started = Instant::now();
                signal.rest(Duration::from_millis(200));
                started.elapsed()
            })
        };
        std::thread::sleep(Duration::from_millis(20));
        signal.raise();

        assert!(
            resting.join().unwrap() >= Duration::from_millis(200),
            "a raise must not cut a rest short"
        );
        assert!(
            signal.is_raised(),
            "a rest must leave a raise for the next wait"
        );
    }

    #[test]
    fn test_signal_stop_ends_a_rest() {
        let signal = Arc::new(Signal::default());
        let (sender, receiver) = mpsc::channel();
        {
            let signal = Arc::clone(&signal);
            std::thread::spawn(move || {
                signal.rest(LONG);
                let _ = sender.send(());
            });
        }
        std::thread::sleep(Duration::from_millis(20));
        signal.stop();
        receiver.recv_timeout(PROMPT).expect("stop must end a rest");
    }

    #[test]
    fn test_signal_stop_wakes_a_waiter_and_stays_stopped() {
        let signal = Arc::new(Signal::default());
        let woken = waiting(&signal);
        std::thread::sleep(Duration::from_millis(20));
        signal.stop();
        woken
            .recv_timeout(PROMPT)
            .expect("stop must wake a waiting thread");
        assert!(signal.is_stopped());

        let started = Instant::now();
        signal.wait(LONG);
        assert!(started.elapsed() < PROMPT, "a stopped signal never blocks");
    }
}

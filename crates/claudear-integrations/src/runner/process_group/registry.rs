use super::signal::Signal;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

static GLOBAL: Registry = Registry::new();

const EMPTIED_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Process groups of agent CLIs that are still running. Each CLI leads its own
/// group, out of reach of the terminal's signals, so claudear passes them on
/// itself and kills whatever is left before it exits.
#[derive(Debug, Default)]
pub struct Registry {
    ids: Mutex<BTreeSet<u32>>,
    interrupted: AtomicBool,
}

impl Registry {
    pub const fn new() -> Self {
        Self {
            ids: Mutex::new(BTreeSet::new()),
            interrupted: AtomicBool::new(false),
        }
    }

    /// The registry every agent run registers its process group in.
    pub fn global() -> &'static Self {
        &GLOBAL
    }

    /// Register group `id`, interrupting it straight away if the registry has
    /// been interrupted: a run can spawn its CLI after claudear was told to stop.
    pub(super) fn insert(&self, id: u32) {
        let mut ids = self.ids();
        ids.insert(id);
        if self.interrupted.load(Ordering::SeqCst) {
            Self::send(id, Signal::Interrupt);
        }
    }

    /// Kill group `id` unless [`Self::kill_all`] already did, so a group id the
    /// OS has since reused is never signalled.
    pub(super) fn kill(&self, id: u32) {
        if self.ids().remove(&id) {
            Self::send(id, Signal::Kill);
        }
    }

    /// Send every group, including any registered later, the SIGINT a terminal
    /// Ctrl-C would, so each CLI can clean up the shells it started in sessions
    /// of their own.
    pub fn interrupt_all(&self) {
        let ids = self.ids();
        self.interrupted.store(true, Ordering::SeqCst);
        for &id in ids.iter() {
            Self::send(id, Signal::Interrupt);
        }
    }

    pub fn kill_all(&self) {
        let ids = std::mem::take(&mut *self.ids());
        for id in ids {
            Self::send(id, Signal::Kill);
        }
    }

    /// Interrupt every group, give their runs up to `grace` to end, then kill
    /// whatever is left.
    pub async fn shutdown(&self, grace: Duration) {
        self.interrupt_all();
        let _ = tokio::time::timeout(grace, self.emptied()).await;
        self.kill_all();
    }

    /// Resolves once every group has been killed, by its run or by
    /// [`Self::kill_all`].
    pub async fn emptied(&self) {
        while !self.is_empty() {
            tokio::time::sleep(EMPTIED_POLL_INTERVAL).await;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ids().is_empty()
    }

    #[cfg(all(test, unix))]
    pub(crate) fn contains(&self, id: u32) -> bool {
        self.ids().contains(&id)
    }

    fn ids(&self) -> MutexGuard<'_, BTreeSet<u32>> {
        self.ids.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(unix)]
    fn send(id: u32, signal: Signal) {
        // Group 0 would be claudear's own.
        let Ok(group @ 1..) = libc::pid_t::try_from(id) else {
            return;
        };
        let number = match signal {
            Signal::Interrupt => libc::SIGINT,
            Signal::Kill => libc::SIGKILL,
        };
        // SAFETY: killpg takes no pointers and only sends a signal.
        if unsafe { libc::killpg(group, number) } == 0 {
            return;
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => {}
            // macOS reports EPERM for a group whose members are all zombies.
            #[cfg(target_os = "macos")]
            Some(libc::EPERM) => tracing::debug!(
                component = "runner",
                process_group = id,
                "Agent process group has only zombies left"
            ),
            _ => tracing::warn!(
                component = "runner",
                process_group = id,
                signal = ?signal,
                error = %error,
                "Failed to signal agent process group"
            ),
        }
    }

    #[cfg(not(unix))]
    fn send(_id: u32, _signal: Signal) {}
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::runner::process_group::tests::{
        assert_interrupted, assert_killed, exits_within, is_running, spawn_group, BACKGROUND_SLEEP,
        EXIT_DEADLINE, INTERRUPTIBLE_LEADER,
    };

    const INTERRUPT_IGNORING_LEADER: &str = "trap '' INT; sleep 300 & echo $!; exec sleep 300";

    #[tokio::test]
    async fn test_kill_all_kills_every_registered_group() {
        let registry = Registry::new();
        let (mut first, first_background) = spawn_group(BACKGROUND_SLEEP).await;
        let (mut second, second_background) = spawn_group(BACKGROUND_SLEEP).await;
        registry.insert(first.id().unwrap());
        registry.insert(second.id().unwrap());

        registry.kill_all();

        for (leader, background) in [
            (&mut first, first_background),
            (&mut second, second_background),
        ] {
            assert_killed(leader).await;
            assert!(
                exits_within(background),
                "background process {background} survived kill_all"
            );
        }
    }

    #[tokio::test]
    async fn test_kill_all_forgets_the_groups_it_killed() {
        let registry = Registry::new();
        let (mut leader, background) = spawn_group(BACKGROUND_SLEEP).await;
        let id = leader.id().unwrap();
        registry.insert(id);

        registry.kill_all();

        assert_killed(&mut leader).await;
        assert!(exits_within(background));
        assert!(
            !registry.contains(id),
            "a killed group must not be signalled again once the OS reuses its id"
        );
    }

    #[tokio::test]
    async fn test_kill_signals_only_registered_groups() {
        let registry = Registry::new();
        let (mut leader, background) = spawn_group(BACKGROUND_SLEEP).await;
        let id = leader.id().unwrap();

        registry.kill(id);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            leader.try_wait().unwrap().is_none(),
            "an unregistered group must not be signalled"
        );

        registry.insert(id);
        registry.kill(id);
        assert_killed(&mut leader).await;
        assert!(
            exits_within(background),
            "background process {background} survived kill"
        );
    }

    #[tokio::test]
    async fn test_interrupt_all_interrupts_without_forgetting_the_group() {
        let registry = Registry::new();
        let (mut leader, background) = spawn_group(INTERRUPTIBLE_LEADER).await;
        let id = leader.id().unwrap();
        registry.insert(id);

        registry.interrupt_all();

        assert_interrupted(&mut leader).await;
        assert!(
            registry.contains(id),
            "kill_all must still reach whatever ignored the interrupt"
        );
        registry.kill_all();
        assert!(exits_within(background));
    }

    #[tokio::test]
    async fn test_groups_registered_after_interrupt_all_are_interrupted() {
        let registry = Registry::new();
        registry.interrupt_all();
        let (mut leader, background) = spawn_group(INTERRUPTIBLE_LEADER).await;

        registry.insert(leader.id().unwrap());

        assert_interrupted(&mut leader).await;
        registry.kill_all();
        assert!(exits_within(background));
    }

    #[tokio::test]
    async fn test_shutdown_kills_what_ignores_the_interrupt() {
        let registry = Registry::new();
        let (mut leader, background) = spawn_group(INTERRUPT_IGNORING_LEADER).await;
        registry.insert(leader.id().unwrap());

        registry.shutdown(Duration::from_millis(200)).await;

        assert_killed(&mut leader).await;
        assert!(exits_within(background));
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn test_shutdown_returns_once_interrupted_runs_end() {
        let registry = Registry::new();
        let (mut leader, background) = spawn_group(INTERRUPTIBLE_LEADER).await;
        let id = leader.id().unwrap();
        registry.insert(id);
        let run_ends_on_interrupt = async {
            assert_interrupted(&mut leader).await;
            registry.kill(id);
        };

        let shutdown = tokio::time::timeout(EXIT_DEADLINE, async {
            tokio::join!(registry.shutdown(2 * EXIT_DEADLINE), run_ends_on_interrupt)
        })
        .await;

        assert!(
            shutdown.is_ok(),
            "shutdown waited out its grace after the run ended"
        );
        assert!(exits_within(background));
    }

    #[tokio::test]
    async fn test_emptied_waits_for_every_group_to_be_killed() {
        let registry = Registry::new();
        let (mut leader, background) = spawn_group(BACKGROUND_SLEEP).await;
        let id = leader.id().unwrap();
        registry.insert(id);

        let early = tokio::time::timeout(Duration::from_millis(200), registry.emptied()).await;
        assert!(early.is_err(), "emptied resolved while a group was live");
        assert!(is_running(background));

        registry.kill(id);

        tokio::time::timeout(EXIT_DEADLINE, registry.emptied())
            .await
            .expect("emptied must resolve once the last group is killed");
        assert_killed(&mut leader).await;
    }
}

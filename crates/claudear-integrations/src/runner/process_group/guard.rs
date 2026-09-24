use super::Registry;
use std::future::Future;
use std::io;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::watch;

/// The process group an agent CLI leads, killed with everything left running in
/// it on [`Self::kill`] or drop. Processes in the group inherit the CLI's stdout
/// and stderr, so until they die a reader never sees EOF.
pub struct Guard<'a> {
    id: Option<u32>,
    registry: &'a Registry,
    killed: watch::Sender<bool>,
}

impl<'a> Guard<'a> {
    /// Spawn `command` as the leader of a new process group, registered in
    /// `registry` until the guard kills it.
    pub fn spawn(command: &mut Command, registry: &'a Registry) -> io::Result<(Child, Self)> {
        #[cfg(unix)]
        command.process_group(0);
        let child = command.kill_on_drop(true).spawn()?;
        let id = child.id();
        if let Some(id) = id {
            registry.insert(id);
        }
        let guard = Self {
            id,
            registry,
            killed: watch::Sender::new(false),
        };
        Ok((child, guard))
    }

    pub fn kill(&mut self) {
        if let Some(id) = self.id.take() {
            self.registry.kill(id);
        }
        self.killed.send_replace(true);
    }

    /// Resolves `grace` after the group is killed or dropped, when readers of
    /// the CLI's output should stop waiting for EOF: a process that moved to a
    /// session of its own escaped the kill and can hold the pipes open forever.
    pub fn drain_deadline(&self, grace: Duration) -> impl Future<Output = ()> + Send + 'static {
        let mut killed = self.killed.subscribe();
        async move {
            let _ = killed.wait_for(|killed| *killed).await;
            tokio::time::sleep(grace).await;
        }
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::runner::process_group::tests::{
        assert_killed, background_pid, exits_within, group_command, BACKGROUND_SLEEP, EXIT_DEADLINE,
    };

    #[tokio::test]
    async fn test_spawn_leads_a_registered_group_of_its_own() {
        let registry = Registry::new();
        let (mut leader, guard) =
            Guard::spawn(&mut group_command(BACKGROUND_SLEEP), &registry).unwrap();
        let id = leader.id().unwrap();
        background_pid(&mut leader).await;

        // SAFETY: getpgid takes no pointers.
        let group = unsafe { libc::getpgid(id as libc::pid_t) };
        assert_eq!(group, id as libc::pid_t, "the CLI must lead its own group");
        assert!(registry.contains(id));
        drop(guard);
    }

    #[tokio::test]
    async fn test_drop_kills_the_group_and_unregisters_it() {
        let registry = Registry::new();
        let (mut leader, guard) =
            Guard::spawn(&mut group_command(BACKGROUND_SLEEP), &registry).unwrap();
        let id = leader.id().unwrap();
        let background = background_pid(&mut leader).await;

        drop(guard);

        assert_killed(&mut leader).await;
        assert!(
            exits_within(background),
            "background process {background} survived the guard's drop"
        );
        assert!(!registry.contains(id), "a dropped guard must unregister");
    }

    #[tokio::test]
    async fn test_kill_all_reaches_a_guarded_group() {
        let registry = Registry::new();
        let (mut leader, _guard) =
            Guard::spawn(&mut group_command(BACKGROUND_SLEEP), &registry).unwrap();
        let background = background_pid(&mut leader).await;

        registry.kill_all();

        assert_killed(&mut leader).await;
        assert!(
            exits_within(background),
            "background process {background} survived kill_all"
        );
    }

    #[tokio::test]
    async fn test_drain_deadline_starts_when_the_group_is_killed() {
        let registry = Registry::new();
        let (mut leader, mut guard) =
            Guard::spawn(&mut group_command(BACKGROUND_SLEEP), &registry).unwrap();
        let deadline = guard.drain_deadline(Duration::ZERO);
        tokio::pin!(deadline);

        let early = tokio::time::timeout(Duration::from_millis(200), &mut deadline).await;
        assert!(early.is_err(), "the deadline passed before the kill");

        guard.kill();

        tokio::time::timeout(EXIT_DEADLINE, deadline)
            .await
            .expect("the deadline must pass once the group is killed");
        assert_killed(&mut leader).await;
    }

    #[tokio::test]
    async fn test_drain_deadline_starts_when_the_guard_drops() {
        let registry = Registry::new();
        let (mut leader, guard) =
            Guard::spawn(&mut group_command(BACKGROUND_SLEEP), &registry).unwrap();
        let deadline = guard.drain_deadline(Duration::ZERO);

        drop(guard);

        tokio::time::timeout(EXIT_DEADLINE, deadline)
            .await
            .expect("the deadline must pass once the guard drops");
        assert_killed(&mut leader).await;
    }
}

//! Heartbeats that keep a live run's attempt out of the orphan sweep.
//!
//! An attempt stays `pending` for as long as its run is live, which can be
//! hours: repository setup, approval and question waits, and agent runs.
//! Every run, in whichever process shares the database, refreshes its
//! attempt's heartbeat while it holds the attempt, and a sweep releases only
//! attempts that have been silent for [`Liveness::stale_after`]. A run that
//! died is recovered within minutes, and a live one is never swept, whatever
//! its timeouts.

use claudear_storage::FixAttemptTracker;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

/// How often a live run refreshes its attempt's heartbeat.
pub const INTERVAL: Duration = Duration::from_secs(60);

/// How many heartbeats in a row a run may miss before a sweep treats its
/// attempt as orphaned, so a run held up by a busy database or runtime is not
/// mistaken for a dead one.
const MISSED_BEATS: u32 = 5;

/// Shortest period heartbeats are sent at, since [`tokio::time::interval`]
/// panics on a zero one.
const SHORTEST_INTERVAL: Duration = Duration::from_millis(1);

/// How often live runs send heartbeats, and how long a run may stay silent
/// before a sweep releases its attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Liveness {
    /// How often a live run refreshes its attempt's heartbeat.
    pub interval: Duration,
    /// How long an attempt may go without a heartbeat before a sweep treats
    /// its run as orphaned.
    pub stale_after: Duration,
}

impl Default for Liveness {
    fn default() -> Self {
        Self {
            interval: INTERVAL,
            stale_after: INTERVAL.saturating_mul(MISSED_BEATS),
        }
    }
}

/// A live run's hold on its attempt, refreshing the attempt's heartbeat until
/// it is dropped.
///
/// The hold is dropped with the run however the run ends, by returning,
/// panicking or being cancelled, so its heartbeat stops with it and an
/// attempt the run did not finish is left for a sweep to release.
pub struct Heartbeat {
    beats: JoinHandle<()>,
}

impl Heartbeat {
    /// Refresh the heartbeat of `issue_id`'s attempt now and every `interval`
    /// until the returned hold is dropped.
    pub fn start(
        tracker: Arc<dyn FixAttemptTracker>,
        source: &str,
        issue_id: &str,
        interval: Duration,
    ) -> Self {
        let source = source.to_string();
        let issue_id = issue_id.to_string();
        let beats = tokio::spawn(async move {
            let mut ticks = tokio::time::interval(interval.max(SHORTEST_INTERVAL));
            ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                ticks.tick().await;
                if let Err(e) = tracker.record_attempt_heartbeat(&source, &issue_id) {
                    tracing::warn!(
                        source = %source,
                        issue_id = %issue_id,
                        error = %e,
                        "Failed to record attempt heartbeat"
                    );
                }
            }
        });
        Self { beats }
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.beats.abort();
    }
}

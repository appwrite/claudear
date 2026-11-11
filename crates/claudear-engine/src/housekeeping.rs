//! Standalone housekeeping worker for periodic background tasks.
//!
//! Runs retries, PR merge cascades, release cascades, auto-close, reviews,
//! learning, regression monitoring, and PR distribution on a timer — independently of the
//! polling loop in [`Watcher::start`].  Used by `Commands::Webhook` and
//! `Commands::Start` (when polling is disabled) so that housekeeping runs
//! regardless of how issues are ingested.

use crate::watcher::Watcher;
use chrono::DateTime;
use chrono::Utc;
use claudear_core::error::Result;
use claudear_integrations::reports::ReportSchedule;
use std::future::Future;
use std::sync::Arc;
use tokio::time::{interval, Duration, Instant};

/// A worker that periodically runs housekeeping tasks via a [`Watcher`].
pub struct HousekeepingWorker {
    watcher: Arc<Watcher>,
    interval_ms: u64,
}

impl HousekeepingWorker {
    /// Create a new worker.  `interval_ms` is clamped to a minimum of 1 000 ms.
    pub fn new(watcher: Arc<Watcher>, interval_ms: u64) -> Self {
        let interval_ms = interval_ms.max(1000);
        Self {
            watcher,
            interval_ms,
        }
    }

    /// Run the housekeeping loop until the watcher is stopped.
    ///
    /// 1. Warm-start (clone repos, sync DB, load feedback).
    /// 2. Mark the watcher as running, or return if it was stopped meanwhile.
    /// 3. On every tick: retries, cascades, auto-close, reviews, metrics,
    ///    periodic learning and report-gen
    ///
    /// Only warm start can fail: the loop logs its failures and carries on.
    pub async fn start(&self) -> anyhow::Result<()> {
        self.watcher.warm_start().await?;
        if self.watcher.mark_running() {
            self.run_loop().await;
        }
        Ok(())
    }

    /// Run the housekeeping tick loop without warm-starting, until the watcher
    /// is stopped.
    ///
    /// Assumes the watcher has already been warm-started and marked as running.
    /// Used by [`Watcher::start`] which handles warm-start itself.
    pub async fn run_loop(&self) {
        let mut timer = interval(Duration::from_millis(self.interval_ms));

        let mut cycle_count: u32 = 0;
        const REFRESH_INTERVAL: u32 = 5;
        const LEARNING_INTERVAL: u32 = 10;

        let reindex_interval = self.watcher.reindex_interval();
        let mut last_reindex = Instant::now();

        let discord_reindex_interval = self.watcher.discord_reindex_interval();
        let mut last_discord_reindex = Instant::now();

        let mut digest_schedule = self.watcher.repetitive_digest_schedule();
        let mut distribution_schedule = self.watcher.pr_distribution_schedule();

        while self.watcher.is_running() {
            timer.tick().await;
            if !self.watcher.is_running() {
                break;
            }
            if self.watcher.is_rate_limit_paused().await {
                self.run_schedules(&mut digest_schedule, &mut distribution_schedule)
                    .await;
                continue;
            }

            cycle_count = cycle_count.wrapping_add(1);
            self.watcher.refresh_user_directory();

            let cron_id = uuid::Uuid::new_v4().to_string();
            let cron_start = Instant::now();
            self.watcher
                .send_cron_check_in("in_progress", &cron_id, None, self.interval_ms);
            if cycle_count.is_multiple_of(REFRESH_INTERVAL) {
                match self.watcher.refresh_repos().await {
                    Ok(0) => {}
                    Ok(n) => {
                        tracing::info!("Discovered and embedded {} new repositories", n);
                        self.watcher.discover_dependencies().await;
                    }
                    Err(e) => tracing::debug!(error = %e, "Error refreshing repos"),
                }
            }

            if let Some(reindex_dur) = reindex_interval {
                if last_reindex.elapsed() >= reindex_dur {
                    self.watcher.pull_and_reindex_all_repos().await;
                    last_reindex = Instant::now();
                }
            }

            if let Some(discord_dur) = discord_reindex_interval {
                if last_discord_reindex.elapsed() >= discord_dur {
                    self.watcher.reindex_discord_knowledgebase().await;
                    last_discord_reindex = Instant::now();
                }
            }

            let auto_close_fut = async {
                if !self.watcher.is_dry_run() {
                    if let Err(e) = self.watcher.check_and_auto_close_prs().await {
                        tracing::debug!(error = %e, "Error checking for auto-close PRs");
                    }
                }
            };

            let reviews_fut = async {
                if !self.watcher.is_dry_run() {
                    if let Err(e) = self.watcher.check_reviews().await {
                        tracing::debug!(error = %e, "Error checking for PR reviews");
                    }
                }
            };

            let housekeeping_fut = async {
                if let Err(e) = self.watcher.run_housekeeping_cycle().await {
                    tracing::error!(component = "housekeeping", error = %e, "Housekeeping error");
                    return false;
                }
                true
            };

            let learning_fut = async {
                if !self.watcher.is_dry_run() && cycle_count.is_multiple_of(LEARNING_INTERVAL) {
                    self.watcher.run_periodic_learning().await;
                }
            };

            let schedules_fut =
                self.run_schedules(&mut digest_schedule, &mut distribution_schedule);

            let (_, _, housekeeping_ok, _, _) = tokio::join!(
                auto_close_fut,
                reviews_fut,
                housekeeping_fut,
                learning_fut,
                schedules_fut
            );

            let duration_secs = cron_start.elapsed().as_secs_f64();
            let cron_status = if housekeeping_ok { "ok" } else { "error" };
            self.watcher.send_cron_check_in(
                cron_status,
                &cron_id,
                Some(duration_secs),
                self.interval_ms,
            );
        }
    }

    /// Send whichever digests are due. They need no Claude run, so they go out
    /// while runs are paused for a rate limit too.
    async fn run_schedules(
        &self,
        digest: &mut Option<ReportSchedule>,
        distribution: &mut Option<ReportSchedule>,
    ) {
        if self.watcher.is_dry_run() {
            return;
        }
        let digest_fut = async {
            if let Some(schedule) = digest.as_mut() {
                let now = Utc::now();
                if schedule.is_due(now) {
                    if let Err(e) = self.watcher.send_repetitive_digest().await {
                        tracing::error!(component = "digest", error = %e, "Error sending repetitive-issues digest");
                    }
                    schedule.last_sent_at = Some(now);
                    self.watcher.record_scheduled_run(schedule);
                }
            }
        };
        let distribution_fut = async {
            if let Some(schedule) = distribution.as_mut() {
                let run = run_when_due(schedule, Utc::now(), || {
                    self.watcher.distribute_pull_requests()
                });
                match run.await {
                    Some(Ok(())) => self.watcher.record_scheduled_run(schedule),
                    Some(Err(e)) => {
                        tracing::error!(component = "pr_distribution", error = %e, "Error distributing open PRs; retrying next tick");
                    }
                    None => {}
                }
            }
        };
        tokio::join!(digest_fut, distribution_fut);
    }
}

/// Run `job` when `schedule` is due at `now`, returning its result, or `None`
/// when it is not due. The slot is marked used only when the job succeeds, so
/// a run that fails is retried on the next tick instead of waiting for the
/// next slot.
async fn run_when_due<F, Fut>(
    schedule: &mut ReportSchedule,
    now: DateTime<Utc>,
    job: F,
) -> Option<Result<()>>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    if !schedule.is_due(now) {
        return None;
    }
    let result = job().await;
    if result.is_ok() {
        schedule.last_sent_at = Some(now);
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use claudear_core::error::Error;

    fn nine_am() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 4, 9, 0, 0).unwrap()
    }

    #[tokio::test]
    async fn a_failed_run_leaves_the_slot_open_for_the_next_tick() {
        let mut schedule = ReportSchedule::daily("pr-distribution", 9);

        let failed = run_when_due(&mut schedule, nine_am(), || async {
            Err(Error::Other("database is locked".to_string()))
        })
        .await;

        assert!(matches!(failed, Some(Err(_))));
        assert!(schedule.last_sent_at.is_none());

        let retried = run_when_due(
            &mut schedule,
            nine_am() + chrono::Duration::minutes(5),
            || async { Ok(()) },
        )
        .await;

        assert!(matches!(retried, Some(Ok(()))));
        assert_eq!(
            schedule.last_sent_at,
            Some(nine_am() + chrono::Duration::minutes(5))
        );
    }

    #[tokio::test]
    async fn a_successful_run_is_not_repeated_in_the_same_slot() {
        let mut schedule = ReportSchedule::daily("pr-distribution", 9);
        let mut runs = 0;

        for minutes in [0, 5] {
            run_when_due(
                &mut schedule,
                nine_am() + chrono::Duration::minutes(minutes),
                || {
                    runs += 1;
                    async { Ok(()) }
                },
            )
            .await;
        }

        assert_eq!(runs, 1);
    }

    #[tokio::test]
    async fn nothing_runs_before_the_slot() {
        let mut schedule = ReportSchedule::daily("pr-distribution", 9);

        let run = run_when_due(
            &mut schedule,
            nine_am() - chrono::Duration::minutes(1),
            || async { panic!("ran before its hour") },
        )
        .await;

        assert!(run.is_none());
    }
}

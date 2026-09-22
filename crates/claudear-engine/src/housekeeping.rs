//! Standalone housekeeping worker for periodic background tasks.
//!
//! Runs retries, PR merge cascades, release cascades, auto-close, reviews,
//! learning, and regression monitoring on a timer — independently of the
//! polling loop in [`Watcher::start`].  Used by `Commands::Webhook` and
//! `Commands::Start` (when polling is disabled) so that housekeeping runs
//! regardless of how issues are ingested.

use crate::watcher::Watcher;
use chrono::Utc;
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
    /// 2. Mark the watcher as running.
    /// 3. On every tick: retries, cascades, auto-close, reviews, metrics,
    ///    periodic learning and report-gen
    pub async fn start(&self) -> anyhow::Result<()> {
        // Warm-start: clone repos, sync to DB, index code, load feedback
        self.watcher.warm_start().await?;
        self.watcher.set_running(true);

        self.run_loop().await
    }

    /// Run the housekeeping tick loop without warm-starting.
    ///
    /// Assumes the watcher has already been warm-started and marked as running.
    /// Used by [`Watcher::start`] which handles warm-start itself.
    pub async fn run_loop(&self) -> anyhow::Result<()> {
        let mut timer = interval(Duration::from_millis(self.interval_ms));
        timer.tick().await; // skip immediate first tick

        let mut cycle_count: u32 = 0;
        const REFRESH_INTERVAL: u32 = 5;
        const LEARNING_INTERVAL: u32 = 10;

        let reindex_interval = self.watcher.reindex_interval();
        let mut last_reindex = Instant::now();

        let discord_reindex_interval = self.watcher.discord_reindex_interval();
        let mut last_discord_reindex = Instant::now();

        // Weekly digest of repetitive, non-actionable Sentry issues. State is
        // held locally in this owned loop (no interior mutability needed). On a
        // process restart `last_sent_at` resets; `is_due` gates on weekday +
        // exact hour + a 6-day window, so a duplicate is only possible if the
        // daemon restarts during the target hour on the target weekday.
        let mut digest_schedule = self.watcher.repetitive_digest_schedule();

        while self.watcher.is_running() {
            timer.tick().await;
            if !self.watcher.is_running() {
                break;
            }
            if self.watcher.is_rate_limit_paused().await {
                continue;
            }

            cycle_count = cycle_count.wrapping_add(1);

            let cron_id = uuid::Uuid::new_v4().to_string();
            let cron_start = Instant::now();
            self.watcher
                .send_cron_check_in("in_progress", &cron_id, None, self.interval_ms);
            // Periodically refresh repo index to detect new repositories
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

            // Periodically pull and re-index all repos
            if let Some(reindex_dur) = reindex_interval {
                if last_reindex.elapsed() >= reindex_dur {
                    self.watcher.pull_and_reindex_all_repos().await;
                    last_reindex = Instant::now();
                }
            }

            // Periodically re-index the Discord knowledge source
            if let Some(discord_dur) = discord_reindex_interval {
                if last_discord_reindex.elapsed() >= discord_dur {
                    self.watcher.reindex_discord_knowledgebase().await;
                    last_discord_reindex = Instant::now();
                }
            }

            // Run independent housekeeping jobs concurrently
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

            // Weekly repetitive-issues digest (report-only).
            let digest_fut = async {
                if !self.watcher.is_dry_run() {
                    if let Some(schedule) = digest_schedule.as_mut() {
                        let now = Utc::now();
                        if schedule.is_due(now) {
                            if let Err(e) = self.watcher.send_repetitive_digest().await {
                                tracing::error!(component = "digest", error = %e, "Error sending repetitive-issues digest");
                            }
                            schedule.last_sent_at = Some(now);
                        }
                    }
                }
            };

            let (_, _, housekeeping_ok, _, _) = tokio::join!(
                auto_close_fut,
                reviews_fut,
                housekeeping_fut,
                learning_fut,
                digest_fut
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

        Ok(())
    }

    /// Signal the watcher to stop.
    pub fn stop(&self) {
        self.watcher.stop();
    }

    /// Signal the watcher to stop and wait for active tasks to drain.
    pub async fn stop_and_drain(&self) {
        self.watcher.stop_and_drain().await;
    }
}

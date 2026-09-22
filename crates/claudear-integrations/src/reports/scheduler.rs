//! Report scheduling logic.

use super::generator::{Report, ReportGenerator};
use crate::notifier::Notifier;
use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Utc, Weekday};
use claudear_core::error::Result;
use claudear_storage::FixAttemptTracker;
use std::sync::Arc;

/// Report frequency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportFrequency {
    /// Daily report
    Daily,
    /// Weekly report (specify day of week)
    Weekly(Weekday),
    /// Monthly report (on the 1st)
    Monthly,
}

impl ReportFrequency {
    /// Parse frequency from string.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "daily" => Some(ReportFrequency::Daily),
            "weekly" => Some(ReportFrequency::Weekly(Weekday::Mon)),
            "weekly-mon" | "weekly-monday" => Some(ReportFrequency::Weekly(Weekday::Mon)),
            "weekly-tue" | "weekly-tuesday" => Some(ReportFrequency::Weekly(Weekday::Tue)),
            "weekly-wed" | "weekly-wednesday" => Some(ReportFrequency::Weekly(Weekday::Wed)),
            "weekly-thu" | "weekly-thursday" => Some(ReportFrequency::Weekly(Weekday::Thu)),
            "weekly-fri" | "weekly-friday" => Some(ReportFrequency::Weekly(Weekday::Fri)),
            "weekly-sat" | "weekly-saturday" => Some(ReportFrequency::Weekly(Weekday::Sat)),
            "weekly-sun" | "weekly-sunday" => Some(ReportFrequency::Weekly(Weekday::Sun)),
            "monthly" => Some(ReportFrequency::Monthly),
            _ => None,
        }
    }

    /// Convert to display string.
    pub fn to_str(&self) -> &'static str {
        match self {
            ReportFrequency::Daily => "daily",
            ReportFrequency::Weekly(Weekday::Mon) => "weekly-monday",
            ReportFrequency::Weekly(Weekday::Tue) => "weekly-tuesday",
            ReportFrequency::Weekly(Weekday::Wed) => "weekly-wednesday",
            ReportFrequency::Weekly(Weekday::Thu) => "weekly-thursday",
            ReportFrequency::Weekly(Weekday::Fri) => "weekly-friday",
            ReportFrequency::Weekly(Weekday::Sat) => "weekly-saturday",
            ReportFrequency::Weekly(Weekday::Sun) => "weekly-sunday",
            ReportFrequency::Monthly => "monthly",
        }
    }
}

/// A report schedule.
#[derive(Debug, Clone)]
pub struct ReportSchedule {
    /// Schedule name
    pub name: String,
    /// How often to send
    pub frequency: ReportFrequency,
    /// Hour to send (0-23 UTC)
    pub hour: u32,
    /// Whether the schedule is enabled
    pub enabled: bool,
    /// Last time a report was sent
    pub last_sent_at: Option<DateTime<Utc>>,
}

impl ReportSchedule {
    /// Create a new daily schedule.
    pub fn daily(name: impl Into<String>, hour: u32) -> Self {
        Self {
            name: name.into(),
            frequency: ReportFrequency::Daily,
            hour,
            enabled: true,
            last_sent_at: None,
        }
    }

    /// Create a new weekly schedule.
    pub fn weekly(name: impl Into<String>, day: Weekday, hour: u32) -> Self {
        Self {
            name: name.into(),
            frequency: ReportFrequency::Weekly(day),
            hour,
            enabled: true,
            last_sent_at: None,
        }
    }

    /// Create a new monthly schedule.
    pub fn monthly(name: impl Into<String>, hour: u32) -> Self {
        Self {
            name: name.into(),
            frequency: ReportFrequency::Monthly,
            hour,
            enabled: true,
            last_sent_at: None,
        }
    }

    /// Check if this schedule is due to run.
    pub fn is_due(&self, now: DateTime<Utc>) -> bool {
        if !self.enabled {
            return false;
        }

        // Check if we're at the right hour
        if now.hour() != self.hour {
            return false;
        }

        // Check frequency-specific conditions
        match self.frequency {
            ReportFrequency::Daily => {
                // Daily: due if we haven't sent today
                match self.last_sent_at {
                    None => true,
                    Some(last) => last.date_naive() < now.date_naive(),
                }
            }
            ReportFrequency::Weekly(day) => {
                // Weekly: due if it's the right day and we haven't sent this week
                if now.weekday() != day {
                    return false;
                }
                match self.last_sent_at {
                    None => true,
                    Some(last) => {
                        // Check if last send was more than 6 days ago
                        (now - last).num_days() >= 6
                    }
                }
            }
            ReportFrequency::Monthly => {
                // Monthly: due on the 1st if we haven't sent this month
                if now.day() != 1 {
                    return false;
                }
                match self.last_sent_at {
                    None => true,
                    Some(last) => last.month() != now.month() || last.year() != now.year(),
                }
            }
        }
    }

    /// Get the next scheduled run time.
    pub fn next_run(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        let target_hour = self.hour;

        match self.frequency {
            ReportFrequency::Daily => {
                let today_target = Utc
                    .with_ymd_and_hms(now.year(), now.month(), now.day(), target_hour, 0, 0)
                    .single()
                    .unwrap_or(now);

                if now < today_target {
                    today_target
                } else {
                    today_target + Duration::days(1)
                }
            }
            ReportFrequency::Weekly(target_day) => {
                let current_day = now.weekday();
                let days_until = (target_day.num_days_from_monday() as i64
                    - current_day.num_days_from_monday() as i64
                    + 7)
                    % 7;

                let target_date = now + Duration::days(days_until);
                let target = Utc
                    .with_ymd_and_hms(
                        target_date.year(),
                        target_date.month(),
                        target_date.day(),
                        target_hour,
                        0,
                        0,
                    )
                    .single()
                    .unwrap_or(now);

                if now < target && days_until == 0 {
                    target
                } else if days_until == 0 {
                    target + Duration::days(7)
                } else {
                    target
                }
            }
            ReportFrequency::Monthly => {
                // Next 1st of the month
                let (year, month) = if now.day() == 1 && now.hour() < target_hour {
                    (now.year(), now.month())
                } else if now.month() == 12 {
                    (now.year() + 1, 1)
                } else {
                    (now.year(), now.month() + 1)
                };

                Utc.with_ymd_and_hms(year, month, 1, target_hour, 0, 0)
                    .single()
                    .unwrap_or(now)
            }
        }
    }
}

/// Manages report schedules and sends reports.
pub struct ReportScheduler {
    generator: ReportGenerator,
    notifier: Arc<dyn Notifier>,
    schedules: Vec<ReportSchedule>,
}

impl ReportScheduler {
    /// Create a new scheduler.
    pub fn new(tracker: Arc<dyn FixAttemptTracker>, notifier: Arc<dyn Notifier>) -> Self {
        Self {
            generator: ReportGenerator::new(tracker),
            notifier,
            schedules: Vec::new(),
        }
    }

    /// Add a schedule.
    pub fn add_schedule(&mut self, schedule: ReportSchedule) {
        self.schedules.push(schedule);
    }

    /// Get all schedules.
    pub fn schedules(&self) -> &[ReportSchedule] {
        &self.schedules
    }

    /// Check and send any due reports.
    pub async fn check_and_send(&mut self) -> Result<Vec<String>> {
        let now = Utc::now();
        let mut sent = Vec::new();

        for schedule in &mut self.schedules {
            if schedule.is_due(now) {
                let report = match schedule.frequency {
                    ReportFrequency::Daily => self.generator.generate_daily()?,
                    ReportFrequency::Weekly(_) => self.generator.generate_weekly()?,
                    ReportFrequency::Monthly => self.generator.generate_monthly()?,
                };

                // Send via notifier
                self.notifier.notify_report(&report).await?;

                schedule.last_sent_at = Some(now);
                sent.push(schedule.name.clone());

                tracing::info!("Sent scheduled report: {}", schedule.name);
            }
        }

        Ok(sent)
    }

    /// Generate a report without sending (preview).
    pub fn preview(&self, frequency: ReportFrequency) -> Result<Report> {
        match frequency {
            ReportFrequency::Daily => self.generator.generate_daily(),
            ReportFrequency::Weekly(_) => self.generator.generate_weekly(),
            ReportFrequency::Monthly => self.generator.generate_monthly(),
        }
    }

    /// Generate and send a report immediately.
    pub async fn send_now(&self, frequency: ReportFrequency) -> Result<Report> {
        let report = self.preview(frequency)?;
        self.notifier.notify_report(&report).await?;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frequency_parse() {
        assert_eq!(
            ReportFrequency::parse("daily"),
            Some(ReportFrequency::Daily)
        );
        assert_eq!(
            ReportFrequency::parse("weekly"),
            Some(ReportFrequency::Weekly(Weekday::Mon))
        );
        assert_eq!(
            ReportFrequency::parse("weekly-friday"),
            Some(ReportFrequency::Weekly(Weekday::Fri))
        );
        assert_eq!(
            ReportFrequency::parse("monthly"),
            Some(ReportFrequency::Monthly)
        );
        assert_eq!(ReportFrequency::parse("invalid"), None);
    }

    #[test]
    fn test_daily_schedule_is_due() {
        let schedule = ReportSchedule::daily("test", 9);

        // At 9am, should be due
        let now = Utc.with_ymd_and_hms(2024, 1, 15, 9, 0, 0).unwrap();
        assert!(schedule.is_due(now));

        // At 10am, should not be due
        let now = Utc.with_ymd_and_hms(2024, 1, 15, 10, 0, 0).unwrap();
        assert!(!schedule.is_due(now));
    }

    #[test]
    fn test_daily_schedule_already_sent() {
        let mut schedule = ReportSchedule::daily("test", 9);
        schedule.last_sent_at = Some(Utc.with_ymd_and_hms(2024, 1, 15, 9, 0, 0).unwrap());

        // Same day, should not be due again
        let now = Utc.with_ymd_and_hms(2024, 1, 15, 9, 30, 0).unwrap();
        assert!(!schedule.is_due(now));

        // Next day, should be due
        let now = Utc.with_ymd_and_hms(2024, 1, 16, 9, 0, 0).unwrap();
        assert!(schedule.is_due(now));
    }

    #[test]
    fn test_weekly_schedule_is_due() {
        let schedule = ReportSchedule::weekly("test", Weekday::Mon, 9);

        // Monday at 9am
        let now = Utc.with_ymd_and_hms(2024, 1, 15, 9, 0, 0).unwrap(); // Monday
        assert!(schedule.is_due(now));

        // Tuesday at 9am
        let now = Utc.with_ymd_and_hms(2024, 1, 16, 9, 0, 0).unwrap(); // Tuesday
        assert!(!schedule.is_due(now));
    }

    #[test]
    fn test_monthly_schedule_is_due() {
        let schedule = ReportSchedule::monthly("test", 9);

        // 1st of month at 9am
        let now = Utc.with_ymd_and_hms(2024, 2, 1, 9, 0, 0).unwrap();
        assert!(schedule.is_due(now));

        // 2nd of month
        let now = Utc.with_ymd_and_hms(2024, 2, 2, 9, 0, 0).unwrap();
        assert!(!schedule.is_due(now));
    }

    #[test]
    fn test_disabled_schedule_not_due() {
        let mut schedule = ReportSchedule::daily("test", 9);
        schedule.enabled = false;

        let now = Utc.with_ymd_and_hms(2024, 1, 15, 9, 0, 0).unwrap();
        assert!(!schedule.is_due(now));
    }

    #[test]
    fn test_next_run_daily() {
        let schedule = ReportSchedule::daily("test", 9);

        // Before target hour
        let now = Utc.with_ymd_and_hms(2024, 1, 15, 8, 0, 0).unwrap();
        let next = schedule.next_run(now);
        assert_eq!(next.hour(), 9);
        assert_eq!(next.day(), 15);

        // After target hour
        let now = Utc.with_ymd_and_hms(2024, 1, 15, 10, 0, 0).unwrap();
        let next = schedule.next_run(now);
        assert_eq!(next.hour(), 9);
        assert_eq!(next.day(), 16);
    }

    #[test]
    fn test_next_run_weekly() {
        let schedule = ReportSchedule::weekly("test", Weekday::Fri, 9);

        // Monday
        let now = Utc.with_ymd_and_hms(2024, 1, 15, 10, 0, 0).unwrap();
        let next = schedule.next_run(now);
        assert_eq!(next.weekday(), Weekday::Fri);
        assert_eq!(next.hour(), 9);
    }

    #[test]
    fn test_frequency_to_str() {
        assert_eq!(ReportFrequency::Daily.to_str(), "daily");
        assert_eq!(ReportFrequency::Monthly.to_str(), "monthly");
        assert_eq!(
            ReportFrequency::Weekly(Weekday::Mon).to_str(),
            "weekly-monday"
        );
        assert_eq!(
            ReportFrequency::Weekly(Weekday::Fri).to_str(),
            "weekly-friday"
        );
        assert_eq!(
            ReportFrequency::Weekly(Weekday::Sun).to_str(),
            "weekly-sunday"
        );
    }

    #[test]
    fn test_frequency_parse_variants() {
        assert_eq!(
            ReportFrequency::parse("DAILY"),
            Some(ReportFrequency::Daily)
        );
        assert_eq!(
            ReportFrequency::parse("Daily"),
            Some(ReportFrequency::Daily)
        );
        assert_eq!(
            ReportFrequency::parse("weekly-tue"),
            Some(ReportFrequency::Weekly(Weekday::Tue))
        );
        assert_eq!(
            ReportFrequency::parse("weekly-wednesday"),
            Some(ReportFrequency::Weekly(Weekday::Wed))
        );
        assert_eq!(
            ReportFrequency::parse("weekly-thursday"),
            Some(ReportFrequency::Weekly(Weekday::Thu))
        );
        assert_eq!(
            ReportFrequency::parse("weekly-saturday"),
            Some(ReportFrequency::Weekly(Weekday::Sat))
        );
    }

    #[test]
    fn test_daily_schedule_wrong_hour() {
        let schedule = ReportSchedule::daily("test", 9);
        let now = Utc.with_ymd_and_hms(2024, 1, 15, 8, 0, 0).unwrap();
        assert!(!schedule.is_due(now));
    }

    #[test]
    fn test_weekly_schedule_wrong_day() {
        let schedule = ReportSchedule::weekly("test", Weekday::Mon, 9);
        // Friday at 9am
        let now = Utc.with_ymd_and_hms(2024, 1, 19, 9, 0, 0).unwrap();
        assert!(!schedule.is_due(now));
    }

    #[test]
    fn test_weekly_schedule_already_sent_this_week() {
        let mut schedule = ReportSchedule::weekly("test", Weekday::Mon, 9);
        // Set last sent to 3 days ago
        schedule.last_sent_at = Some(Utc.with_ymd_and_hms(2024, 1, 12, 9, 0, 0).unwrap());

        // Today is Monday Jan 15
        let now = Utc.with_ymd_and_hms(2024, 1, 15, 9, 0, 0).unwrap();
        assert!(!schedule.is_due(now));
    }

    #[test]
    fn test_monthly_schedule_wrong_day() {
        let schedule = ReportSchedule::monthly("test", 9);
        // 15th of the month
        let now = Utc.with_ymd_and_hms(2024, 1, 15, 9, 0, 0).unwrap();
        assert!(!schedule.is_due(now));
    }

    #[test]
    fn test_monthly_schedule_already_sent_this_month() {
        let mut schedule = ReportSchedule::monthly("test", 9);
        schedule.last_sent_at = Some(Utc.with_ymd_and_hms(2024, 1, 1, 9, 0, 0).unwrap());

        // Same month, 1st day
        let now = Utc.with_ymd_and_hms(2024, 1, 1, 9, 30, 0).unwrap();
        assert!(!schedule.is_due(now));
    }

    #[test]
    fn test_monthly_schedule_new_month() {
        let mut schedule = ReportSchedule::monthly("test", 9);
        schedule.last_sent_at = Some(Utc.with_ymd_and_hms(2024, 1, 1, 9, 0, 0).unwrap());

        // February 1st
        let now = Utc.with_ymd_and_hms(2024, 2, 1, 9, 0, 0).unwrap();
        assert!(schedule.is_due(now));
    }

    #[test]
    fn test_next_run_daily_same_day() {
        let schedule = ReportSchedule::daily("test", 18);
        let now = Utc.with_ymd_and_hms(2024, 1, 15, 10, 0, 0).unwrap();
        let next = schedule.next_run(now);
        assert_eq!(next.day(), 15); // Same day
        assert_eq!(next.hour(), 18);
    }

    #[test]
    fn test_next_run_monthly() {
        let schedule = ReportSchedule::monthly("test", 9);
        // Mid-month
        let now = Utc.with_ymd_and_hms(2024, 1, 15, 10, 0, 0).unwrap();
        let next = schedule.next_run(now);
        assert_eq!(next.day(), 1);
        assert_eq!(next.month(), 2);
    }

    #[test]
    fn test_next_run_monthly_year_rollover() {
        let schedule = ReportSchedule::monthly("test", 9);
        // December 15
        let now = Utc.with_ymd_and_hms(2024, 12, 15, 10, 0, 0).unwrap();
        let next = schedule.next_run(now);
        assert_eq!(next.day(), 1);
        assert_eq!(next.month(), 1);
        assert_eq!(next.year(), 2025);
    }

    #[test]
    fn test_schedule_name_field() {
        let schedule = ReportSchedule::daily("My Daily Report", 9);
        assert_eq!(schedule.name, "My Daily Report");
    }

    #[test]
    fn test_schedule_enabled_field() {
        let mut schedule = ReportSchedule::daily("test", 9);
        assert!(schedule.enabled);
        schedule.enabled = false;
        assert!(!schedule.enabled);
    }

    #[test]
    fn test_schedule_last_sent_at_field() {
        let mut schedule = ReportSchedule::daily("test", 9);
        assert!(schedule.last_sent_at.is_none());
        schedule.last_sent_at = Some(Utc::now());
        assert!(schedule.last_sent_at.is_some());
    }

    #[test]
    fn test_report_frequency_eq() {
        assert_eq!(ReportFrequency::Daily, ReportFrequency::Daily);
        assert_eq!(
            ReportFrequency::Weekly(Weekday::Mon),
            ReportFrequency::Weekly(Weekday::Mon)
        );
        assert_ne!(
            ReportFrequency::Weekly(Weekday::Mon),
            ReportFrequency::Weekly(Weekday::Tue)
        );
        assert_ne!(ReportFrequency::Daily, ReportFrequency::Monthly);
    }

    #[test]
    fn test_report_frequency_copy() {
        let freq = ReportFrequency::Daily;
        let freq_copy = freq;
        assert_eq!(freq, freq_copy);
    }

    #[test]
    fn test_frequency_to_str_all_days() {
        assert_eq!(
            ReportFrequency::Weekly(Weekday::Tue).to_str(),
            "weekly-tuesday"
        );
        assert_eq!(
            ReportFrequency::Weekly(Weekday::Wed).to_str(),
            "weekly-wednesday"
        );
        assert_eq!(
            ReportFrequency::Weekly(Weekday::Thu).to_str(),
            "weekly-thursday"
        );
        assert_eq!(
            ReportFrequency::Weekly(Weekday::Sat).to_str(),
            "weekly-saturday"
        );
    }

    #[test]
    fn test_frequency_parse_mon_short() {
        assert_eq!(
            ReportFrequency::parse("weekly-mon"),
            Some(ReportFrequency::Weekly(Weekday::Mon))
        );
    }

    #[test]
    fn test_frequency_parse_sun_variants() {
        assert_eq!(
            ReportFrequency::parse("weekly-sun"),
            Some(ReportFrequency::Weekly(Weekday::Sun))
        );
        assert_eq!(
            ReportFrequency::parse("weekly-sunday"),
            Some(ReportFrequency::Weekly(Weekday::Sun))
        );
    }

    #[test]
    fn test_frequency_parse_monday_full() {
        assert_eq!(
            ReportFrequency::parse("weekly-monday"),
            Some(ReportFrequency::Weekly(Weekday::Mon))
        );
    }

    #[test]
    fn test_next_run_weekly_same_day_before_target_hour() {
        // Friday schedule, currently Friday before target hour
        let schedule = ReportSchedule::weekly("test", Weekday::Fri, 14);
        let now = Utc.with_ymd_and_hms(2024, 1, 19, 10, 0, 0).unwrap(); // Friday 10am
        let next = schedule.next_run(now);
        assert_eq!(next.weekday(), Weekday::Fri);
        assert_eq!(next.hour(), 14);
        assert_eq!(next.day(), 19); // Same day
    }

    #[test]
    fn test_next_run_weekly_same_day_after_target_hour() {
        // Friday schedule, currently Friday after target hour
        let schedule = ReportSchedule::weekly("test", Weekday::Fri, 9);
        let now = Utc.with_ymd_and_hms(2024, 1, 19, 15, 0, 0).unwrap(); // Friday 3pm
        let next = schedule.next_run(now);
        assert_eq!(next.weekday(), Weekday::Fri);
        assert_eq!(next.hour(), 9);
        assert_eq!(next.day(), 26); // Next Friday
    }

    #[test]
    fn test_next_run_monthly_on_first_before_hour() {
        let schedule = ReportSchedule::monthly("test", 14);
        // On the 1st at 10am, target is 2pm
        let now = Utc.with_ymd_and_hms(2024, 3, 1, 10, 0, 0).unwrap();
        let next = schedule.next_run(now);
        assert_eq!(next.day(), 1);
        assert_eq!(next.month(), 3); // Same month
        assert_eq!(next.hour(), 14);
    }

    #[test]
    fn test_next_run_monthly_on_first_after_hour() {
        let schedule = ReportSchedule::monthly("test", 9);
        // On the 1st at 3pm, target is 9am (already past)
        let now = Utc.with_ymd_and_hms(2024, 3, 1, 15, 0, 0).unwrap();
        let next = schedule.next_run(now);
        assert_eq!(next.day(), 1);
        assert_eq!(next.month(), 4); // Next month
        assert_eq!(next.hour(), 9);
    }

    #[test]
    fn test_weekly_schedule_sent_long_ago_is_due() {
        let mut schedule = ReportSchedule::weekly("test", Weekday::Mon, 9);
        // Last sent 14 days ago
        schedule.last_sent_at = Some(Utc.with_ymd_and_hms(2024, 1, 1, 9, 0, 0).unwrap());

        // Monday Jan 15 at 9am
        let now = Utc.with_ymd_and_hms(2024, 1, 15, 9, 0, 0).unwrap();
        assert!(schedule.is_due(now));
    }

    #[test]
    fn test_monthly_schedule_different_year() {
        let mut schedule = ReportSchedule::monthly("test", 9);
        // Last sent Dec 2023
        schedule.last_sent_at = Some(Utc.with_ymd_and_hms(2023, 12, 1, 9, 0, 0).unwrap());

        // Jan 1 2024 at 9am
        let now = Utc.with_ymd_and_hms(2024, 1, 1, 9, 0, 0).unwrap();
        assert!(schedule.is_due(now));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn test_scheduler_add_and_get_schedules() {
        use claudear_storage::SqliteTracker;
        use std::sync::Arc;

        struct MockNotifier;

        #[async_trait::async_trait]
        impl crate::notifier::Notifier for MockNotifier {
            fn name(&self) -> &str {
                "mock"
            }
            fn is_enabled(&self) -> bool {
                true
            }
            async fn notify_start(
                &self,
                _: &claudear_core::types::Issue,
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            async fn notify_success(
                &self,
                _: &claudear_core::types::Issue,
                _: &str,
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            async fn notify_completed(
                &self,
                _: &claudear_core::types::Issue,
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            async fn notify_failed(
                &self,
                _: &claudear_core::types::Issue,
                _: &str,
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            async fn notify_status(&self, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            async fn notify_urgent_issues(
                &self,
                _: &[claudear_core::types::Issue],
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
        }

        let tracker: Arc<dyn claudear_storage::FixAttemptTracker> =
            Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier: Arc<dyn crate::notifier::Notifier> = Arc::new(MockNotifier);

        let mut scheduler = ReportScheduler::new(tracker, notifier);
        assert!(scheduler.schedules().is_empty());

        scheduler.add_schedule(ReportSchedule::daily("daily-report", 9));
        scheduler.add_schedule(ReportSchedule::weekly("weekly-report", Weekday::Mon, 10));

        assert_eq!(scheduler.schedules().len(), 2);
        assert_eq!(scheduler.schedules()[0].name, "daily-report");
        assert_eq!(scheduler.schedules()[1].name, "weekly-report");
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn test_scheduler_preview() {
        use claudear_storage::SqliteTracker;
        use std::sync::Arc;

        struct MockNotifier;

        #[async_trait::async_trait]
        impl crate::notifier::Notifier for MockNotifier {
            fn name(&self) -> &str {
                "mock"
            }
            fn is_enabled(&self) -> bool {
                true
            }
            async fn notify_start(
                &self,
                _: &claudear_core::types::Issue,
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            async fn notify_success(
                &self,
                _: &claudear_core::types::Issue,
                _: &str,
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            async fn notify_completed(
                &self,
                _: &claudear_core::types::Issue,
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            async fn notify_failed(
                &self,
                _: &claudear_core::types::Issue,
                _: &str,
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            async fn notify_status(&self, _: &str) -> claudear_core::error::Result<()> {
                Ok(())
            }
            async fn notify_urgent_issues(
                &self,
                _: &[claudear_core::types::Issue],
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
        }

        let tracker: Arc<dyn claudear_storage::FixAttemptTracker> =
            Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier: Arc<dyn crate::notifier::Notifier> = Arc::new(MockNotifier);

        let scheduler = ReportScheduler::new(tracker, notifier);

        // Preview daily report
        let report = scheduler.preview(ReportFrequency::Daily).unwrap();
        assert!(report.period.contains("24 Hours"));

        // Preview weekly report
        let report = scheduler
            .preview(ReportFrequency::Weekly(Weekday::Mon))
            .unwrap();
        assert!(report.period.contains("7 Days"));

        // Preview monthly report
        let report = scheduler.preview(ReportFrequency::Monthly).unwrap();
        assert!(report.period.contains("30 Days"));
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_scheduler_send_now() {
        use claudear_storage::SqliteTracker;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct CountingNotifier {
            call_count: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl crate::notifier::Notifier for CountingNotifier {
            fn name(&self) -> &str {
                "counting"
            }
            fn is_enabled(&self) -> bool {
                true
            }
            async fn notify_start(
                &self,
                _: &claudear_core::types::Issue,
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            async fn notify_success(
                &self,
                _: &claudear_core::types::Issue,
                _: &str,
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            async fn notify_completed(
                &self,
                _: &claudear_core::types::Issue,
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            async fn notify_failed(
                &self,
                _: &claudear_core::types::Issue,
                _: &str,
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            async fn notify_status(&self, _: &str) -> claudear_core::error::Result<()> {
                self.call_count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            async fn notify_urgent_issues(
                &self,
                _: &[claudear_core::types::Issue],
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
        }

        let tracker: Arc<dyn claudear_storage::FixAttemptTracker> =
            Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(CountingNotifier {
            call_count: AtomicUsize::new(0),
        });
        let notifier_clone = Arc::clone(&notifier);
        let notifier_trait: Arc<dyn crate::notifier::Notifier> = notifier;

        let scheduler = ReportScheduler::new(tracker, notifier_trait);

        let report = scheduler.send_now(ReportFrequency::Daily).await.unwrap();
        assert!(report.period.contains("24 Hours"));
        // notify_report calls notify_status by default
        assert_eq!(notifier_clone.call_count.load(Ordering::SeqCst), 1);
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_scheduler_check_and_send_due_schedule() {
        use claudear_storage::SqliteTracker;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct CountingNotifier {
            call_count: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl crate::notifier::Notifier for CountingNotifier {
            fn name(&self) -> &str {
                "counting"
            }
            fn is_enabled(&self) -> bool {
                true
            }
            async fn notify_start(
                &self,
                _: &claudear_core::types::Issue,
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            async fn notify_success(
                &self,
                _: &claudear_core::types::Issue,
                _: &str,
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            async fn notify_completed(
                &self,
                _: &claudear_core::types::Issue,
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            async fn notify_failed(
                &self,
                _: &claudear_core::types::Issue,
                _: &str,
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
            async fn notify_status(&self, _: &str) -> claudear_core::error::Result<()> {
                self.call_count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            async fn notify_urgent_issues(
                &self,
                _: &[claudear_core::types::Issue],
            ) -> claudear_core::error::Result<()> {
                Ok(())
            }
        }

        let tracker: Arc<dyn claudear_storage::FixAttemptTracker> =
            Arc::new(SqliteTracker::in_memory().unwrap());
        let notifier = Arc::new(CountingNotifier {
            call_count: AtomicUsize::new(0),
        });
        let notifier_clone = Arc::clone(&notifier);
        let notifier_trait: Arc<dyn crate::notifier::Notifier> = notifier;

        let mut scheduler = ReportScheduler::new(tracker, notifier_trait);

        // Add a daily schedule at the current hour so it's due
        let current_hour = Utc::now().hour();
        scheduler.add_schedule(ReportSchedule::daily("auto-daily", current_hour));

        let sent = scheduler.check_and_send().await.unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0], "auto-daily");
        assert_eq!(notifier_clone.call_count.load(Ordering::SeqCst), 1);

        // Calling again should not send (already sent today)
        let sent = scheduler.check_and_send().await.unwrap();
        assert!(sent.is_empty());
    }

    #[test]
    fn test_next_run_weekly_different_day() {
        // Schedule for Wednesday, currently Monday
        let schedule = ReportSchedule::weekly("test", Weekday::Wed, 10);
        let now = Utc.with_ymd_and_hms(2024, 1, 15, 8, 0, 0).unwrap(); // Monday
        let next = schedule.next_run(now);
        assert_eq!(next.weekday(), Weekday::Wed);
        assert_eq!(next.hour(), 10);
    }
}

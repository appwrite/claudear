//! Scheduled reports module.
//!
//! Provides automated daily/weekly reporting via notifications.

mod generator;
mod scheduler;

pub use generator::{RecurringIssue, RepetitiveDigest, RepetitiveEntry, Report, ReportGenerator};
pub use scheduler::{ReportFrequency, ReportSchedule, ReportScheduler};

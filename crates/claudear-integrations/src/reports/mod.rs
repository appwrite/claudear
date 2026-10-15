//! Scheduled reports module.
//!
//! Provides automated daily/weekly reporting via notifications.

mod generator;
mod scheduler;
mod support;

pub use generator::{RecurringIssue, RepetitiveDigest, RepetitiveEntry, Report, ReportGenerator};
pub use scheduler::{ReportFrequency, ReportSchedule, ReportScheduler};
pub use support::{
    is_solved, Speaker, SupportDigest, SupportEntry, SupportMessage, SupportStatus, SupportThread,
};

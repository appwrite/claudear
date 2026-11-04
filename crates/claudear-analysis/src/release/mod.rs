//! Release tracking for bug fix verification.
//!
//! This module tracks releases across the Appwrite ecosystem to detect
//! when bug fixes are included in production releases.
//!
//! Supports transitive release tracking through dependency chains.

mod github;
mod tracker;

pub use github::GitHubRelease;
pub use github::GitHubTag;
pub use github::PrDetails;
pub use github::ReleaseAuthor;
pub use github::ReleaseClient;
pub use github::BODY_LIMIT;
pub use tracker::ReleaseTracker;
pub use tracker::ReleaseTrackerConfig;

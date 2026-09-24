//! Release tracking for bug fix verification.
//!
//! This module tracks releases across the Appwrite ecosystem to detect
//! when bug fixes are included in production releases.
//!
//! Supports transitive release tracking through dependency chains.

mod github;
mod tracker;

pub use github::{GitHubRelease, GitHubTag, PrDetails, ReleaseAuthor, ReleaseClient};
pub use tracker::{ReleaseTracker, ReleaseTrackerConfig};

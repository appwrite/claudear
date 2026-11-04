//! Release tracking for bug fix verification.
//!
//! This module tracks releases across the Appwrite ecosystem to detect
//! when bug fixes are included in production releases.
//!
//! Supports transitive release tracking through dependency chains.

mod deployment;
mod github;
mod tracker;

pub use deployment::image_references;
pub use deployment::locked_package;
pub use deployment::DeployedApplication;
pub use deployment::DeployedPackage;
pub use deployment::DeploymentResolver;
pub use deployment::DeploymentStatus;
pub use deployment::DeploymentTopology;
pub use deployment::ImageForm;
pub use deployment::ImageReference;
pub use deployment::LiveStatus;
pub use deployment::LockedPackage;
pub use deployment::PullRequestState;
pub use deployment::RepositoryRole;
pub use github::GitHubRelease;
pub use github::GitHubTag;
pub use github::PrDetails;
pub use github::ReleaseAuthor;
pub use github::ReleaseClient;
pub use github::BODY_LIMIT;
pub use tracker::ReleaseTracker;
pub use tracker::ReleaseTrackerConfig;

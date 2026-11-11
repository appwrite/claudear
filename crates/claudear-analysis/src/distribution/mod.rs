//! Distribution of Claudear's open PRs across developers.
//!
//! Every open Claudear PR gets one developer responsible for getting it over
//! the line. Candidates are scored by how closely they own the PR
//! ([`Evidence`]), then [`Balancer`] hands each PR to its strongest owner who is
//! still under the fair share of open PRs, so one prolific owner never ends up
//! holding everything.

mod balancer;
mod codeowners;
mod developer;
mod notice;
mod ownership;
mod pull_request;
mod roster;
mod wrapped_up;

pub use balancer::Balancer;
pub use balancer::Decision;
pub use balancer::Pending;
pub use codeowners::CodeOwners;
pub use developer::Developer;
pub use notice::AssignmentNotice;
pub use notice::Digest;
pub use notice::DigestGroup;
pub use notice::Markup;
pub use notice::PlainText;
pub use notice::PullRequestSummary;
pub use notice::MENTION_BREAK;
pub use ownership::Candidate;
pub use ownership::Evidence;
pub use pull_request::Attention;
pub use pull_request::PullRequest;
pub use pull_request::PullRequestState;
pub use pull_request::Review;
pub use pull_request::Verdict;
pub use roster::Roster;
pub use wrapped_up::WrappedUp;

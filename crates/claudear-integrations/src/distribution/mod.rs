//! Making one developer responsible for each open Claudear PR.
//!
//! [`Distributor`] assigns a PR as soon as Claudear opens it, and on a
//! schedule reconciles every open Claudear PR: it closes assignments of
//! merged or closed PRs, hands PRs of developers who left the roster to
//! someone else, assigns any PR still without an owner, and sends each
//! assignee a digest of the PRs waiting on them.

mod distributor;
mod host;

pub use distributor::Distributor;
pub use distributor::Outcome;
pub use host::PullRequestHost;

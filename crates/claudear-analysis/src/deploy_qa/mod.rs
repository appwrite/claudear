//! Live deploy QA for new GitHub release tips.
//!
//! Distinct from [`crate::release::ReleaseTracker`] / `[regression]`, which
//! watch **bug-fix inclusion**. This module durable-watches **new tips** on
//! configured tracks, persists last-seen + attempt status, and enqueues an
//! observe/report agent run (no fix PRs).

mod map;
mod playbook;
mod probe;
mod tracker;

pub use map::{GitHubDiscordMap, MappedDiscordUser};
pub use playbook::{bundled_playbook, load_playbook, DEPLOY_QA_SOURCE};
pub use probe::{classify_deploy_qa_verdict, DeployQaVerdict, LiveQaProbe, NoopLiveQaProbe};
pub use tracker::{
    build_deploy_qa_issue, deploy_qa_match_result, DeployQaPollAction, DeployQaPollResult,
    DeployQaTracker, ReleaseTip, OBSERVE_ONLY_METADATA_KEY, REPO_METADATA_KEY, TAG_METADATA_KEY,
    TRACK_METADATA_KEY,
};

//! Synthetic `deploy_qa` issue source.
//!
//! Surfaces pending tips persisted by [`claudear_analysis::deploy_qa::DeployQaTracker`].
//! Observe/report only — `add_comment` posts Discord `#releases` outcomes and
//! never opens a fix PR.

use super::IssueSource;
use crate::deploy_qa::{report_deploy_qa_outcome, DeployQaDiscord};
use async_trait::async_trait;
use claudear_analysis::deploy_qa::{
    build_deploy_qa_issue, deploy_qa_match_result, load_playbook, ReleaseTip, DEPLOY_QA_SOURCE,
    TAG_METADATA_KEY, TRACK_METADATA_KEY,
};
use claudear_config::config::{DeployQaConfig, DeployQaTrackConfig};
use claudear_core::error::Result;
use claudear_core::types::{DeployQaTip, Issue, MatchPriority, MatchResult};
use claudear_storage::FixAttemptTracker;
use std::path::Path;
use std::sync::Arc;

const DISCORD_MESSAGE_METADATA_KEY: &str = "discord_message_id";

/// Issue source that drains pending `[deploy_qa]` tips from SQLite.
pub struct DeployQaSource {
    config: DeployQaConfig,
    store: Arc<dyn FixAttemptTracker>,
    playbook: String,
    discord: Option<DeployQaDiscord>,
}

impl DeployQaSource {
    /// Create a source that reads pending tips from `store`.
    pub fn new(
        config: DeployQaConfig,
        store: Arc<dyn FixAttemptTracker>,
        discord: Option<DeployQaDiscord>,
    ) -> Result<Self> {
        let playbook = load_playbook(config.instructions_path.as_ref().map(Path::new))?;
        Ok(Self {
            config,
            store,
            playbook,
            discord,
        })
    }

    fn track_for(&self, name: &str, repo: &str) -> DeployQaTrackConfig {
        self.config
            .tracks
            .iter()
            .find(|t| t.name == name)
            .cloned()
            .unwrap_or(DeployQaTrackConfig {
                name: name.to_string(),
                repo: repo.to_string(),
                tag_filter: Default::default(),
            })
    }

    fn issue_for_tip(&self, tip: &DeployQaTip) -> Issue {
        let track = self.track_for(&tip.track, &tip.repo);
        let release = ReleaseTip {
            repo: tip.repo.clone(),
            tag: tip.tag.clone(),
            name: Some(tip.tag.clone()),
            body: tip.release_body.clone(),
            published_at: tip.published_at.clone(),
            html_url: tip.html_url.clone().unwrap_or_else(|| {
                format!("https://github.com/{}/releases/tag/{}", tip.repo, tip.tag)
            }),
            author_login: tip.author_login.clone(),
        };
        let mut issue = build_deploy_qa_issue(&track, &release, &self.playbook);
        issue.id = tip.issue_id.clone();
        if let Some(ref message_id) = tip.discord_message_id {
            issue.set_metadata(DISCORD_MESSAGE_METADATA_KEY, message_id.clone());
        }
        issue
    }
}

#[async_trait]
impl IssueSource for DeployQaSource {
    fn name(&self) -> &str {
        DEPLOY_QA_SOURCE
    }

    fn display_name(&self) -> &str {
        "Deploy QA"
    }

    async fn fetch_issues(&self) -> Result<Vec<Issue>> {
        Ok(self
            .store
            .list_pending_deploy_qa_tips()?
            .iter()
            .map(|tip| self.issue_for_tip(tip))
            .collect())
    }

    fn matches_criteria(&self, issue: &Issue) -> MatchResult {
        let track = issue
            .get_metadata::<String>(TRACK_METADATA_KEY)
            .unwrap_or_default();
        let tag = issue
            .get_metadata::<String>(TAG_METADATA_KEY)
            .unwrap_or_default();
        if track.is_empty() {
            return MatchResult::matched("deploy_qa pending tip", MatchPriority::High);
        }
        deploy_qa_match_result(&track, &tag)
    }

    async fn build_issue_context(&self, issue: &Issue) -> Result<String> {
        Ok(issue.description.clone().unwrap_or_else(|| {
            format!(
                "{}\n\nObserve/report only. Do not open a fix PR.",
                self.playbook
            )
        }))
    }

    async fn get_issue(&self, issue_id: &str) -> Result<Issue> {
        let tip = self
            .store
            .get_deploy_qa_tip_by_issue_id(issue_id)?
            .ok_or_else(|| {
                claudear_core::error::Error::Other(format!("deploy_qa tip {issue_id} not found"))
            })?;
        Ok(self.issue_for_tip(&tip))
    }

    async fn add_comment(&self, issue_id: &str, comment: &str) -> Result<()> {
        report_deploy_qa_outcome(
            self.store.as_ref(),
            self.discord.as_ref(),
            issue_id,
            comment,
        )
        .await
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudear_analysis::deploy_qa::OBSERVE_ONLY_METADATA_KEY;
    use claudear_core::types::DeployQaTipStatus;
    use claudear_storage::SqliteTracker;

    #[tokio::test]
    async fn fetch_issues_returns_pending_tips_only() {
        let sqlite = SqliteTracker::in_memory().unwrap();
        let mut pending = DeployQaTip::new("cloud", "appwrite-labs/cloud", "1.0.0");
        pending.release_body = Some("Adds #1".into());
        let stored = sqlite.upsert_deploy_qa_tip(&pending).unwrap();

        let mut running = DeployQaTip::new("cloud", "appwrite-labs/cloud", "0.9.0");
        running.status = DeployQaTipStatus::Running;
        sqlite.upsert_deploy_qa_tip(&running).unwrap();

        let store: Arc<dyn FixAttemptTracker> = Arc::new(sqlite);
        let source = DeployQaSource::new(DeployQaConfig::default(), store, None).unwrap();
        let issues = source.fetch_issues().await.unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].id, stored.issue_id);
        assert_eq!(issues[0].source, DEPLOY_QA_SOURCE);
        assert_eq!(
            issues[0].get_metadata::<bool>(OBSERVE_ONLY_METADATA_KEY),
            Some(true)
        );
        assert!(source.matches_criteria(&issues[0]).matches);
        let context = source.build_issue_context(&issues[0]).await.unwrap();
        assert!(context.contains(pending.release_body.as_deref().unwrap()));
    }

    #[tokio::test]
    async fn fetch_issues_and_get_issue_agree_for_same_tip() {
        let sqlite = SqliteTracker::in_memory().unwrap();
        let mut tip = DeployQaTip::new("cloud", "appwrite-labs/cloud", "1.0.0");
        tip.html_url = None;
        tip.discord_message_id = Some("1234567890".into());
        let stored = sqlite.upsert_deploy_qa_tip(&tip).unwrap();

        let store: Arc<dyn FixAttemptTracker> = Arc::new(sqlite);
        let source = DeployQaSource::new(DeployQaConfig::default(), store, None).unwrap();
        let fetched = source.fetch_issues().await.unwrap().remove(0);
        let resolved = source.get_issue(&stored.issue_id).await.unwrap();

        assert_eq!(fetched.id, resolved.id);
        assert_eq!(fetched.url, resolved.url);
        assert!(!resolved.url.is_empty());
        for issue in [&fetched, &resolved] {
            assert_eq!(
                issue.get_metadata::<String>(DISCORD_MESSAGE_METADATA_KEY),
                stored.discord_message_id
            );
        }
    }
}

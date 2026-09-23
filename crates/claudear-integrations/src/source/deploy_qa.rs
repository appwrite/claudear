//! Synthetic `deploy_qa` issue source.
//!
//! Surfaces pending tips persisted by [`claudear_analysis::deploy_qa::DeployQaTracker`].
//! Observe/report only — `add_comment` posts Discord `#releases` outcomes and
//! never opens a fix PR.

use super::IssueSource;
use crate::deploy_qa::{report_deploy_qa_outcome, DeployQaDiscord};
use async_trait::async_trait;
use claudear_analysis::deploy_qa::{
    build_deploy_qa_issue, deploy_qa_match_result, load_playbook, DEPLOY_QA_SOURCE,
};
use claudear_config::config::{DeployQaConfig, DeployQaTrackConfig};
use claudear_core::error::Result;
use claudear_core::types::{Issue, MatchPriority, MatchResult};
use claudear_storage::FixAttemptTracker;
use std::path::Path;
use std::sync::Arc;

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
        let pending = self.store.list_pending_deploy_qa_tips()?;
        let mut issues = Vec::with_capacity(pending.len());
        for tip in pending {
            let track = self.track_for(&tip.track, &tip.repo);
            let release = claudear_analysis::deploy_qa::ReleaseTip {
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
            let mut issue = build_deploy_qa_issue(&track, &release, &self.playbook, &self.config);
            if let Some(ref message_id) = tip.discord_message_id {
                issue.set_metadata("discord_message_id", message_id.clone());
            }
            issues.push(issue);
        }
        Ok(issues)
    }

    fn matches_criteria(&self, issue: &Issue) -> MatchResult {
        let track = issue
            .get_metadata::<String>("deploy_qa_track")
            .unwrap_or_default();
        let tag = issue
            .get_metadata::<String>("deploy_qa_tag")
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
        let track = self.track_for(&tip.track, &tip.repo);
        let release = claudear_analysis::deploy_qa::ReleaseTip {
            repo: tip.repo.clone(),
            tag: tip.tag.clone(),
            name: Some(tip.tag.clone()),
            body: tip.release_body.clone(),
            published_at: tip.published_at.clone(),
            html_url: tip.html_url.clone().unwrap_or_default(),
            author_login: tip.author_login.clone(),
        };
        Ok(build_deploy_qa_issue(
            &track,
            &release,
            &self.playbook,
            &self.config,
        ))
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
    use claudear_core::types::{DeployQaTip, DeployQaTipStatus};
    use claudear_storage::SqliteTracker;

    #[tokio::test]
    async fn fetch_issues_returns_pending_tips_only() {
        let sqlite = SqliteTracker::in_memory().unwrap();
        let mut pending = DeployQaTip::new("cloud", "appwrite-labs/cloud", "1.0.0");
        pending.release_body = Some("Adds #1".into());
        sqlite.upsert_deploy_qa_tip(&pending).unwrap();

        let mut running = DeployQaTip::new("cloud", "appwrite-labs/cloud", "0.9.0");
        running.status = DeployQaTipStatus::Running;
        sqlite.upsert_deploy_qa_tip(&running).unwrap();

        let store: Arc<dyn FixAttemptTracker> = Arc::new(sqlite);
        let source = DeployQaSource::new(DeployQaConfig::default(), store, None).unwrap();
        let issues = source.fetch_issues().await.unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].id, "appwrite-labs/cloud:1.0.0");
        assert_eq!(issues[0].source, DEPLOY_QA_SOURCE);
        assert!(source.matches_criteria(&issues[0]).matches);
        let ctx = source.build_issue_context(&issues[0]).await.unwrap();
        assert!(ctx.contains("Adds #1"));
        assert!(ctx.contains("Do **not** open a fix PR"));
    }
}

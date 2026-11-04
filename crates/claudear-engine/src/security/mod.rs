//! Security inbox: screens, assesses, deduplicates and tracks vulnerability
//! reports from the security mailbox, replying with fixed templates only.
//!
//! Report text is untrusted input. It only ever reaches an agent inside
//! [`prompt::UntrustedText`] markers and a sandboxed `security_query`, and
//! every agent-produced field passes through [`scrub_internal`] before it is
//! stored, notified, replied or handed to the fix pipeline.

mod assessment;
mod dedup;
mod fix;
mod history;
mod intake;
mod lifecycle;
mod notify;
mod prompt;
mod replies;
mod screening;
mod scrub;

#[cfg(test)]
mod tests;

pub use scrub::scrub_internal;

use async_trait::async_trait;
use claudear_analysis::feedback::EmbeddingClient;
use claudear_analysis::inference::RepoInferrer;
use claudear_analysis::release::DeploymentStatus;
use claudear_config::config::{Config, SecurityInboxConfig};
use claudear_core::error::Result;
use claudear_core::security::SECURITY_FIX_SOURCE;
use claudear_core::types::{Issue, VerifyResult};
use claudear_integrations::notifier::Notifier;
use claudear_integrations::runner::AgentRunner;
use claudear_integrations::source::IssueSource;
use claudear_storage::FixAttemptTracker;
use std::sync::Arc;
use std::time::Duration;

use dedup::TextEmbedder;

const HELPSCOUT_SOURCE: &str = "helpscout";
const MAILBOX_ID_METADATA_KEY: &str = "mailbox_id";
const SENDING_STALE_AFTER: Duration = Duration::from_secs(10 * 60);
const REPLY_RETRY_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// Whether `issue` belongs to the security inbox and must never reach a
/// generic pipeline. A HelpScout issue without a mailbox id is treated as
/// security whenever a security mailbox is configured.
pub fn is_security_issue(config: &Config, issue: &Issue) -> bool {
    if issue.source == SECURITY_FIX_SOURCE {
        return true;
    }
    issue.source == HELPSCOUT_SOURCE
        && config.helpscout().is_some_and(|helpscout| {
            helpscout.treats_as_security(
                issue
                    .get_metadata::<String>(MAILBOX_ID_METADATA_KEY)
                    .as_deref(),
            )
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityPollDecision {
    /// Hand the conversation to [`SecurityInbox::route`].
    Route,
    /// Only record a reporter follow-up with [`SecurityInbox::observe_follow_up`].
    FollowUp,
    Skip,
}

pub struct SecurityInboxDependencies {
    pub config: SecurityInboxConfig,
    pub tracker: Arc<dyn FixAttemptTracker>,
    pub agent: Arc<dyn AgentRunner>,
    pub embedding_client: Option<Arc<EmbeddingClient>>,
    pub notifier: Arc<dyn Notifier>,
    pub source: Arc<dyn IssueSource>,
    pub deployment: Option<Arc<dyn DeploymentStatus>>,
    pub inferrer: Option<RepoInferrer>,
}

/// A neutral fix run for one vulnerability. `issue` carries no report text,
/// reporter or conversation reference; `context` and `diagnosis` hold only
/// scrubbed assessment fields.
#[derive(Debug, Clone)]
pub struct SecurityFixRequest {
    pub vulnerability_id: i64,
    pub repository: Option<String>,
    pub issue: Issue,
    pub context: String,
    pub diagnosis: VerifyResult,
}

/// Context provider for a security fix run: the sanitized brief, and no
/// channel back to the reporter.
pub struct SecurityFixContext {
    pub context: String,
}

#[async_trait]
impl crate::processing::ContextProvider for SecurityFixContext {
    async fn build_issue_context(&self, _issue: &Issue) -> Result<String> {
        Ok(self.context.clone())
    }

    async fn post_reply(&self, _issue_id: &str, _body: &str) -> Result<()> {
        Err(claudear_core::error::Error::Other(
            "security fixes never reply to the reporter".to_string(),
        ))
    }
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum SecurityRoute {
    Done { reason: String },
    RunFix(SecurityFixRequest),
}

impl SecurityRoute {
    fn done(reason: impl Into<String>) -> Self {
        Self::Done {
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SecurityAdvanceSummary {
    pub released: usize,
    pub replies_sent: usize,
    pub merged: usize,
    pub live: usize,
    pub awards_prepared: usize,
    pub needs_human: usize,
    pub errors: usize,
}

pub struct SecurityInbox {
    config: SecurityInboxConfig,
    tracker: Arc<dyn FixAttemptTracker>,
    agent: Arc<dyn AgentRunner>,
    embedder: Option<Arc<dyn TextEmbedder>>,
    notifier: Arc<dyn Notifier>,
    source: Arc<dyn IssueSource>,
    deployment: Option<Arc<dyn DeploymentStatus>>,
    inferrer: Option<RepoInferrer>,
    registration: tokio::sync::Mutex<()>,
    sending_stale_after: Duration,
    reply_retry_backoff: Duration,
    stale_assessment_after: Duration,
}

impl SecurityInbox {
    pub fn new(dependencies: SecurityInboxDependencies) -> Self {
        let stale_assessment_after =
            assessment::assessment_deadline(dependencies.config.assessment_timeout());
        Self {
            config: dependencies.config,
            tracker: dependencies.tracker,
            agent: dependencies.agent,
            embedder: dependencies
                .embedding_client
                .map(|client| client as Arc<dyn TextEmbedder>),
            notifier: dependencies.notifier,
            source: dependencies.source,
            deployment: dependencies.deployment,
            inferrer: dependencies.inferrer,
            registration: tokio::sync::Mutex::new(()),
            sending_stale_after: SENDING_STALE_AFTER,
            reply_retry_backoff: REPLY_RETRY_BACKOFF,
            stale_assessment_after,
        }
    }

    #[cfg(test)]
    fn with_embedder(mut self, embedder: Arc<dyn TextEmbedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    #[cfg(test)]
    fn with_sending_stale_after(mut self, stale_after: Duration) -> Self {
        self.sending_stale_after = stale_after;
        self
    }

    #[cfg(test)]
    fn with_stale_assessment_after(mut self, stale_after: Duration) -> Self {
        self.stale_assessment_after = stale_after;
        self
    }

    #[cfg(test)]
    fn with_reply_retry_backoff(mut self, backoff: Duration) -> Self {
        self.reply_retry_backoff = backoff;
        self
    }
}

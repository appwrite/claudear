//! Feature status API.
//!
//! Reports, for every config section, whether claudear is running with it
//! configured and enabled, a hand-picked set of non-secret settings, and
//! whether each secret is set. Secret values never leave the process.
//!
//! Every signed-in user can read this, so settings exclude filesystem paths,
//! hosts, URLs and free-form queries; admins see those in the config API.

use super::auth::AuthUser;
use super::routes::ApiState;
use axum::{extract::State, Json};
use claudear_config::config::{Config, ProviderConfig};
use claudear_core::secret::SecretValue;
use serde::Serialize;
use serde_json::Value;

const BUILDERS: &[fn(&Config) -> FeatureStatus] = &[
    linear,
    sentry,
    jira,
    discord_source,
    slack_source,
    helpscout_source,
    github_code_scanning,
    github,
    gitlab,
    discord_notifier,
    slack_notifier,
    email,
    sms,
    push,
    whatsapp,
    telegram,
    helpscout_replies,
    agent,
    agent_experiments,
    ask,
    retry,
    regression,
    deploy_qa,
    cascade,
    evaluation,
    prioritisation,
    reports,
    learning,
    code_index,
    retrieval_eval,
    embedding,
    llm,
    chat,
    qa,
    discord_knowledgebase,
    dashboard,
    tls,
    mcp_server,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureCategory {
    Source,
    Scm,
    Notifier,
    Automation,
    Intelligence,
    Platform,
}

#[derive(Debug, Serialize)]
pub struct FeatureSetting {
    pub name: String,
    pub value: Value,
}

#[derive(Debug, Serialize)]
pub struct FeatureSecret {
    pub name: String,
    pub set: bool,
}

/// `enabled` is never true unless `configured` is.
#[derive(Debug, Serialize)]
pub struct FeatureStatus {
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub category: FeatureCategory,
    pub configured: bool,
    pub enabled: bool,
    pub settings: Vec<FeatureSetting>,
    pub secrets: Vec<FeatureSecret>,
}

#[derive(Debug, Serialize)]
pub struct FeaturesResponse {
    pub features: Vec<FeatureStatus>,
}

impl FeatureStatus {
    fn new(
        key: &'static str,
        label: &'static str,
        description: &'static str,
        category: FeatureCategory,
    ) -> Self {
        Self {
            key,
            label,
            description,
            category,
            configured: true,
            enabled: true,
            settings: Vec::new(),
            secrets: Vec::new(),
        }
    }

    fn state(mut self, configured: bool, enabled: bool) -> Self {
        self.configured = configured;
        self.enabled = configured && enabled;
        self
    }

    fn unconfigured(self) -> Self {
        self.state(false, false)
    }

    fn setting(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.settings.push(FeatureSetting {
            name: name.into(),
            value: value.into(),
        });
        self
    }

    fn secret(self, name: impl Into<String>, value: Option<&SecretValue>) -> Self {
        self.secret_presence(name, is_set(value))
    }

    fn plain_secret(self, name: impl Into<String>, value: Option<&str>) -> Self {
        let set = value.is_some_and(|secret| !secret.trim().is_empty());
        self.secret_presence(name, set)
    }

    fn secret_presence(mut self, name: impl Into<String>, set: bool) -> Self {
        self.secrets.push(FeatureSecret {
            name: name.into(),
            set,
        });
        self
    }
}

/// `GET /api/features` — the running configuration's feature status.
pub async fn features_handler(
    _user: AuthUser,
    State(state): State<ApiState>,
) -> Json<FeaturesResponse> {
    Json(FeaturesResponse {
        features: features(&state.config),
    })
}

pub fn features(config: &Config) -> Vec<FeatureStatus> {
    BUILDERS.iter().map(|build| build(config)).collect()
}

fn is_set(value: Option<&SecretValue>) -> bool {
    value.is_some_and(|secret| !secret.expose().trim().is_empty())
}

fn serialised(value: &impl Serialize) -> Value {
    serde_json::to_value(value).unwrap_or_default()
}

fn linear(config: &Config) -> FeatureStatus {
    let feature = FeatureStatus::new(
        "issues.linear",
        "Linear",
        "Picks up Linear issues that carry a trigger label or state.",
        FeatureCategory::Source,
    );
    let Some(linear) = config.issues.linear.as_ref() else {
        return feature.unconfigured();
    };
    feature
        .state(true, linear.enabled)
        .setting("trigger_labels", linear.trigger_labels.clone())
        .setting("trigger_states", linear.trigger_states.clone())
        .setting("trigger_assignee", linear.trigger_assignee.clone())
        .setting("team_id", linear.team_id.clone())
        .setting("project_id", linear.project_id.clone())
        .setting(
            "poll_interval_ms",
            linear.poll_interval_ms.unwrap_or(config.poll_interval_ms),
        )
        .secret("api_key", Some(&linear.api_key))
        .secret("webhook_secret", linear.webhook_secret.as_ref())
}

fn sentry(config: &Config) -> FeatureStatus {
    let feature = FeatureStatus::new(
        "issues.sentry",
        "Sentry",
        "Fixes the top Sentry issues by event count.",
        FeatureCategory::Source,
    );
    let Some(sentry) = config.issues.sentry.as_ref() else {
        return feature.unconfigured();
    };
    feature
        .state(true, sentry.enabled)
        .setting("org_slug", sentry.org_slug.clone())
        .setting("project_slugs", sentry.project_slugs.clone())
        .setting("top_issues_count", sentry.top_issues_count)
        .setting("top_issues_period", sentry.top_issues_period.to_string())
        .setting("min_event_count", sentry.min_event_count)
        .setting(
            "escalation_threshold_percent",
            sentry.escalation_threshold_percent,
        )
        .setting(
            "poll_interval_ms",
            sentry.poll_interval_ms.unwrap_or(config.poll_interval_ms),
        )
        .secret("auth_token", Some(&sentry.auth_token))
        .secret("client_secret", sentry.client_secret.as_ref())
}

fn jira(config: &Config) -> FeatureStatus {
    let feature = FeatureStatus::new(
        "issues.jira",
        "Jira",
        "Picks up Jira issues matching the trigger labels and statuses.",
        FeatureCategory::Source,
    );
    let Some(jira) = config.issues.jira.as_ref() else {
        return feature.unconfigured();
    };
    feature
        .state(true, jira.enabled)
        .setting("auth_mode", jira.auth_mode.clone())
        .setting("project_keys", jira.project_keys.clone())
        .setting("trigger_labels", jira.trigger_labels.clone())
        .setting("trigger_statuses", jira.trigger_statuses.clone())
        .setting("issue_types", jira.issue_types.clone())
        .setting("max_results", jira.max_results)
        .setting(
            "poll_interval_ms",
            jira.poll_interval_ms.unwrap_or(config.poll_interval_ms),
        )
        .secret("api_token", Some(&jira.api_token))
}

fn discord_source(config: &Config) -> FeatureStatus {
    let feature = FeatureStatus::new(
        "issues.discord",
        "Discord issues",
        "Turns Discord channel messages into issues and questions.",
        FeatureCategory::Source,
    );
    let Some(discord) = config.issues.discord.as_ref() else {
        return feature.unconfigured();
    };
    feature
        .state(true, true)
        .setting("channel_id", discord.channel_id.clone())
        .setting("listen_channel_id", discord.listen_channel_id.clone())
        .setting("guild_id", discord.guild_id.clone())
        .setting("mention_only", discord.bot_id.is_some())
        .setting(
            "reply_chain_enabled",
            discord.reply_chain_enabled.unwrap_or(true),
        )
        .setting(
            "reply_chain_max_depth",
            discord.reply_chain_max_depth.unwrap_or(15),
        )
        .setting(
            "poll_interval_ms",
            discord.poll_interval_ms.unwrap_or(config.poll_interval_ms),
        )
        .secret("bot_token", discord.bot_token.as_ref())
}

fn slack_source(config: &Config) -> FeatureStatus {
    let feature = FeatureStatus::new(
        "issues.slack",
        "Slack issues",
        "Turns Slack channel messages into issues and questions.",
        FeatureCategory::Source,
    );
    let Some(slack) = config.issues.slack.as_ref() else {
        return feature.unconfigured();
    };
    feature
        .state(true, true)
        .setting("channel_id", slack.channel_id.clone())
        .setting("listen_channel_id", slack.listen_channel_id.clone())
        .setting("app_id", slack.app_id.clone())
        .setting(
            "poll_interval_ms",
            slack.poll_interval_ms.unwrap_or(config.poll_interval_ms),
        )
        .secret("bot_token", slack.bot_token.as_ref())
        .secret("signing_secret", slack.signing_secret.as_ref())
        .secret("app_config_token", slack.app_config_token.as_ref())
}

fn helpscout_source(config: &Config) -> FeatureStatus {
    let feature = FeatureStatus::new(
        "issues.helpscout",
        "HelpScout",
        "Picks up tagged HelpScout support conversations.",
        FeatureCategory::Source,
    );
    let Some(helpscout) = config.issues.helpscout.as_ref() else {
        return feature.unconfigured();
    };
    feature
        .state(true, helpscout.enabled)
        .setting("mailbox_ids", helpscout.mailbox_ids.clone())
        .setting("trigger_tags", helpscout.trigger_tags.clone())
        .setting("trigger_status", helpscout.trigger_status.clone())
        .setting("reply_as", serialised(&helpscout.reply_as))
        .setting(
            "poll_interval_ms",
            helpscout
                .poll_interval_ms
                .unwrap_or(config.poll_interval_ms),
        )
        .secret("app_id", Some(&helpscout.app_id))
        .secret("app_secret", Some(&helpscout.app_secret))
        .secret("webhook_secret", helpscout.webhook_secret.as_ref())
}

fn github_code_scanning(config: &Config) -> FeatureStatus {
    let feature = FeatureStatus::new(
        "issues.github_code_scanning",
        "GitHub code scanning",
        "Fixes GitHub code-scanning alerts using the GitHub credentials.",
        FeatureCategory::Source,
    );
    let Some(scanning) = config.issues.github_code_scanning.as_ref() else {
        return feature.unconfigured();
    };
    let severities: Vec<&str> = scanning
        .severities
        .iter()
        .map(|severity| severity.as_str())
        .collect();
    feature
        .state(true, scanning.enabled)
        .setting("repos", scanning.repos.clone())
        .setting("tool_names", scanning.tool_names.clone())
        .setting("severities", severities)
        .setting(
            "poll_interval_ms",
            scanning.poll_interval_ms.unwrap_or(config.poll_interval_ms),
        )
}

fn github(config: &Config) -> FeatureStatus {
    let github = &config.scm.github;
    let app = &github.app;
    FeatureStatus::new(
        "scm.github",
        "GitHub",
        "Opens pull requests, follows reviews and picks up GitHub issues.",
        FeatureCategory::Scm,
    )
    .state(config.is_github_enabled(), true)
    .setting("app_configured", app.is_configured())
    .setting("repos", github.repos.clone())
    .setting("trigger_labels", github.trigger_labels.clone())
    .setting("trigger_states", github.trigger_states.clone())
    .setting("review_trigger", github.review_trigger.clone())
    .setting("allowed_bots", github.allowed_bots.clone())
    .setting("auto_resolve_on_merge", github.auto_resolve_on_merge)
    .setting("use_ssh", github.use_ssh)
    .setting("poll_interval_ms", github.poll_interval_ms)
    .setting("app.app_id", app.app_id)
    .setting("app.installation_id", app.installation_id)
    .setting("app.client_id", app.client_id.clone())
    .secret("token", github.token.as_ref())
    .secret("webhook_secret", github.webhook_secret.as_ref())
    .secret("app.private_key", app.private_key.as_ref())
    .secret("app.webhook_secret", app.webhook_secret.as_ref())
    .secret("app.client_secret", app.client_secret.as_ref())
}

fn gitlab(config: &Config) -> FeatureStatus {
    let feature = FeatureStatus::new(
        "scm.gitlab",
        "GitLab",
        "Opens merge requests and picks up GitLab issues.",
        FeatureCategory::Scm,
    );
    let Some(gitlab) = config.scm.gitlab.as_ref() else {
        return feature.unconfigured();
    };
    feature
        .state(true, config.is_gitlab_enabled())
        .setting("groups", gitlab.groups.clone())
        .setting("trigger_labels", gitlab.trigger_labels.clone())
        .setting("trigger_states", gitlab.trigger_states.clone())
        .setting("review_trigger", gitlab.review_trigger.clone())
        .setting("allowed_bots", gitlab.allowed_bots.clone())
        .setting("auto_resolve_on_merge", gitlab.auto_resolve_on_merge)
        .setting("use_ssh", gitlab.use_ssh)
        .setting(
            "poll_interval_ms",
            gitlab.poll_interval_ms.unwrap_or(config.poll_interval_ms),
        )
        .secret("token", gitlab.token.as_ref())
        .secret("webhook_secret", gitlab.webhook_secret.as_ref())
}

fn discord_notifier(config: &Config) -> FeatureStatus {
    let discord = &config.notifiers.discord;
    let configured = is_set(discord.webhook_url.as_ref()) || is_set(discord.bot_token.as_ref());
    FeatureStatus::new(
        "notifiers.discord",
        "Discord notifications",
        "Posts attempt updates to Discord and reads replies.",
        FeatureCategory::Notifier,
    )
    .state(configured, true)
    .setting("user_id", discord.user_id.clone())
    .setting("channel_id", discord.channel_id.clone())
    .setting("guild_id", discord.guild_id.clone())
    .secret("webhook_url", discord.webhook_url.as_ref())
    .secret("bot_token", discord.bot_token.as_ref())
}

fn slack_notifier(config: &Config) -> FeatureStatus {
    let slack = &config.notifiers.slack;
    let configured = is_set(slack.webhook_url.as_ref()) || is_set(slack.bot_token.as_ref());
    FeatureStatus::new(
        "notifiers.slack",
        "Slack notifications",
        "Posts attempt updates to Slack and reads replies.",
        FeatureCategory::Notifier,
    )
    .state(configured, true)
    .setting("user_id", slack.user_id.clone())
    .setting("channel_id", slack.channel_id.clone())
    .secret("webhook_url", slack.webhook_url.as_ref())
    .secret("bot_token", slack.bot_token.as_ref())
}

fn email(config: &Config) -> FeatureStatus {
    let email = &config.notifiers.email;
    let configured = email.smtp_host.is_some() || email.imap_host.is_some();
    FeatureStatus::new(
        "notifiers.email",
        "Email",
        "Sends notifications over SMTP and reads replies over IMAP.",
        FeatureCategory::Notifier,
    )
    .state(configured, true)
    .setting("smtp_port", email.smtp_port)
    .setting("use_tls", email.use_tls)
    .setting("recipients", email.to_addresses.len())
    .setting("imap_port", email.imap_port)
    .setting("imap_use_tls", email.imap_use_tls)
    .setting("imap_folder", email.imap_folder.clone())
    .secret("smtp_password", email.smtp_password.as_ref())
    .secret("imap_password", email.imap_password.as_ref())
}

fn sms(config: &Config) -> FeatureStatus {
    let sms = &config.notifiers.sms;
    let configured = sms.account_sid.is_some() && is_set(sms.auth_token.as_ref());
    FeatureStatus::new(
        "notifiers.sms",
        "SMS",
        "Sends notifications as text messages through Twilio.",
        FeatureCategory::Notifier,
    )
    .state(configured, true)
    .setting("recipients", sms.to_numbers.len())
    .plain_secret("account_sid", sms.account_sid.as_deref())
    .secret("auth_token", sms.auth_token.as_ref())
}

fn push(config: &Config) -> FeatureStatus {
    let push = &config.notifiers.push;
    let configured = is_set(push.api_token.as_ref()) && push.user_key.is_some();
    FeatureStatus::new(
        "notifiers.push",
        "Push",
        "Sends push notifications through Pushover.",
        FeatureCategory::Notifier,
    )
    .state(configured, true)
    .setting("device", push.device.clone())
    .setting("priority", push.priority)
    .secret("api_token", push.api_token.as_ref())
    .plain_secret("user_key", push.user_key.as_deref())
}

fn whatsapp(config: &Config) -> FeatureStatus {
    let whatsapp = &config.notifiers.whatsapp;
    let configured = whatsapp.phone_number_id.is_some() && is_set(whatsapp.access_token.as_ref());
    FeatureStatus::new(
        "notifiers.whatsapp",
        "WhatsApp",
        "Sends notifications through WhatsApp Business, optionally reading messages as issues.",
        FeatureCategory::Notifier,
    )
    .state(configured, true)
    .setting("recipients", whatsapp.to_numbers.len())
    .setting("source_enabled", whatsapp.source_enabled)
    .setting(
        "poll_interval_ms",
        whatsapp.poll_interval_ms.unwrap_or(config.poll_interval_ms),
    )
    .secret("access_token", whatsapp.access_token.as_ref())
    .secret("app_secret", whatsapp.app_secret.as_ref())
    .secret(
        "webhook_verify_token",
        whatsapp.webhook_verify_token.as_ref(),
    )
}

fn telegram(config: &Config) -> FeatureStatus {
    let telegram = &config.notifiers.telegram;
    FeatureStatus::new(
        "notifiers.telegram",
        "Telegram",
        "Sends notifications through a Telegram bot, optionally reading messages as issues.",
        FeatureCategory::Notifier,
    )
    .state(is_set(telegram.bot_token.as_ref()), true)
    .setting("chat_id", telegram.chat_id.clone())
    .setting("recipients", telegram.to_chat_ids.len())
    .setting("source_enabled", telegram.source_enabled)
    .setting("listen_chat_id", telegram.listen_chat_id.clone())
    .setting(
        "poll_interval_ms",
        telegram.poll_interval_ms.unwrap_or(config.poll_interval_ms),
    )
    .secret("bot_token", telegram.bot_token.as_ref())
}

fn helpscout_replies(config: &Config) -> FeatureStatus {
    let reply = config.reply();
    FeatureStatus::new(
        "notifiers.helpscout",
        "HelpScout replies",
        "Classifies, verifies and replies to HelpScout conversations.",
        FeatureCategory::Notifier,
    )
    .state(config.issues.helpscout.is_some(), reply.enabled)
    .setting("sound_human", reply.sound_human)
    .setting("default_template", reply.default_template.is_some())
    .setting("templates", reply.templates.len())
    .setting("verify_timeout_secs", reply.verify_timeout_secs)
    .setting("verify_fail_open", reply.verify_fail_open)
}

fn agent(config: &Config) -> FeatureStatus {
    let agent = &config.agent;
    let mut providers: Vec<(&String, &ProviderConfig)> = agent.providers.iter().collect();
    providers.sort_by_key(|(name, _)| *name);
    let mut mcp_servers: Vec<&String> = agent
        .providers
        .values()
        .flat_map(|provider| provider.mcp.keys())
        .collect();
    mcp_servers.sort();
    mcp_servers.dedup();
    let default_provider = agent.default_provider_config();
    let feature = FeatureStatus::new(
        "agent",
        "Agent",
        "The coding agent that fixes issues and answers questions.",
        FeatureCategory::Automation,
    )
    .state(default_provider.is_some(), true)
    .setting("default_provider", agent.default_provider.clone())
    .setting(
        "model",
        default_provider.and_then(|provider| provider.model.clone()),
    )
    .setting("timeout_secs", agent.timeout_secs)
    .setting("use_llm", agent.use_llm)
    .setting(
        "providers",
        providers
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
    )
    .setting(
        "mcp_servers",
        mcp_servers
            .iter()
            .map(|name| name.as_str())
            .collect::<Vec<_>>(),
    );
    providers
        .into_iter()
        .fold(feature, |feature, (name, provider)| {
            feature.secret(
                format!("providers.{name}.api_key"),
                provider.api_key.as_ref(),
            )
        })
}

fn agent_experiments(config: &Config) -> FeatureStatus {
    let experiments = &config.agent.experiments;
    let active: Vec<&str> = experiments
        .iter()
        .filter(|experiment| experiment.enabled)
        .map(|experiment| experiment.name.as_str())
        .collect();
    FeatureStatus::new(
        "agent.experiments",
        "Provider experiments",
        "Splits agent runs across providers to compare them.",
        FeatureCategory::Automation,
    )
    .state(!experiments.is_empty(), !active.is_empty())
    .setting("experiments", experiments.len())
    .setting("active", active)
}

fn ask(config: &Config) -> FeatureStatus {
    let ask = &config.ask;
    FeatureStatus::new(
        "ask",
        "Ask a human",
        "Lets the agent pause and ask a person when it is unsure.",
        FeatureCategory::Automation,
    )
    .state(true, ask.enabled)
    .setting("wait_timeout_secs", ask.wait_timeout_secs)
    .setting("poll_interval_secs", ask.poll_interval_secs)
    .setting("max_rounds_per_attempt", ask.max_rounds_per_attempt)
    .setting("best_effort_on_timeout", ask.best_effort_on_timeout)
    .setting("require_approval", ask.require_approval)
}

fn retry(config: &Config) -> FeatureStatus {
    let retry = &config.retry;
    FeatureStatus::new(
        "retry",
        "Retries",
        "Retries failed attempts with exponential backoff.",
        FeatureCategory::Automation,
    )
    .state(true, retry.max_retries > 0)
    .setting("max_retries", retry.max_retries)
    .setting("base_delay_ms", retry.base_delay_ms)
    .setting("max_delay_ms", retry.max_delay_ms)
}

fn regression(config: &Config) -> FeatureStatus {
    let regression = &config.regression;
    FeatureStatus::new(
        "regression",
        "Regression watch",
        "Watches merged fixes for the error coming back.",
        FeatureCategory::Automation,
    )
    .state(true, regression.enabled)
    .setting(
        "check_interval_secs",
        regression.effective_check_interval_secs(),
    )
    .setting(
        "monitoring_duration_secs",
        regression.effective_monitoring_duration_secs(),
    )
    .setting("sentry_event_threshold", regression.sentry_event_threshold)
    .setting("similarity_threshold", regression.similarity_threshold)
    .setting("target_repos", regression.target_repos.clone())
    .setting(
        "github_search_repos",
        regression.github_search_repos.clone(),
    )
    .plain_secret("github_token", regression.github_token.as_deref())
}

fn deploy_qa(config: &Config) -> FeatureStatus {
    let deploy_qa = &config.deploy_qa;
    FeatureStatus::new(
        "deploy_qa",
        "Deploy QA",
        "Runs a live QA pass against each new GitHub release.",
        FeatureCategory::Automation,
    )
    .state(!deploy_qa.tracks.is_empty(), deploy_qa.enabled)
    .setting("tracks", deploy_qa.tracks.len())
    .setting("poll_interval_ms", deploy_qa.effective_poll_interval_ms())
    .setting("timeout_secs", deploy_qa.effective_timeout_secs())
    .setting(
        "skip_if_previous_running",
        deploy_qa.skip_if_previous_running,
    )
    .setting("discord_channel_id", deploy_qa.discord_channel_id.clone())
}

fn cascade(config: &Config) -> FeatureStatus {
    let cascade = &config.cascade;
    FeatureStatus::new(
        "cascade",
        "Cascade",
        "Chains follow-up changes into downstream repositories.",
        FeatureCategory::Automation,
    )
    .state(!cascade.rules.is_empty(), cascade.enabled)
    .setting("rules", cascade.rules.len())
    .setting("max_depth", cascade.max_depth)
}

fn evaluation(config: &Config) -> FeatureStatus {
    let evaluation = &config.evaluation;
    FeatureStatus::new(
        "evaluation",
        "Self-evaluation",
        "Measures test, lint, analysis and coverage deltas for each fix.",
        FeatureCategory::Automation,
    )
    .state(true, evaluation.enabled)
    .setting("test_delta", evaluation.test_delta)
    .setting("lint_delta", evaluation.lint_delta)
    .setting("static_analysis_delta", evaluation.static_analysis_delta)
    .setting("coverage_delta", evaluation.coverage_delta)
    .setting("post_pr_comment", evaluation.post_pr_comment)
    .setting("fail_on_regression", evaluation.fail_on_regression)
    .setting("require_red_green", evaluation.require_red_green)
    .setting("total_timeout_secs", evaluation.total_timeout_secs)
}

fn prioritisation(config: &Config) -> FeatureStatus {
    let prioritisation = &config.prioritisation;
    FeatureStatus::new(
        "prioritisation",
        "Prioritisation",
        "Scores issues by severity, frequency, regressions and blast radius.",
        FeatureCategory::Automation,
    )
    .state(true, prioritisation.enabled)
    .setting("severity_weight", prioritisation.severity_weight)
    .setting("frequency_weight", prioritisation.frequency_weight)
    .setting("regression_weight", prioritisation.regression_weight)
    .setting("blast_radius_weight", prioritisation.blast_radius_weight)
    .setting("cluster_weight", prioritisation.cluster_weight)
    .setting("content_clustering", prioritisation.content_clustering)
    .setting("suppression_rules", prioritisation.suppression_rules.len())
}

fn reports(config: &Config) -> FeatureStatus {
    let digest = &config.reports.repetitive_digest;
    FeatureStatus::new(
        "reports.repetitive_digest",
        "Repetitive issue digest",
        "Sends a weekly digest of issues that keep recurring.",
        FeatureCategory::Automation,
    )
    .state(true, digest.enabled)
    .setting("day", digest.day.clone())
    .setting("hour", digest.hour)
    .setting("min_event_count", digest.min_event_count)
}

fn learning(config: &Config) -> FeatureStatus {
    let learning = &config.learning;
    FeatureStatus::new(
        "learning",
        "Continuous learning",
        "Extracts learnings from attempts, reviews and answers.",
        FeatureCategory::Intelligence,
    )
    .setting("auto_extract_learnings", learning.auto_extract_learnings)
    .setting("diff_analysis", learning.diff_analysis)
    .setting("qa_promotion", learning.qa_promotion)
    .setting("repo_knowledge", learning.repo_knowledge)
    .setting("review_classification", learning.review_classification)
    .setting("strategy_fingerprinting", learning.strategy_fingerprinting)
    .setting("quality_scoring", learning.quality_scoring)
    .setting("cluster_detection", learning.cluster_detection)
    .setting("auto_agent_md", learning.auto_agent_md)
    .setting("cross_repo_correlation", learning.cross_repo_correlation)
}

fn code_index(config: &Config) -> FeatureStatus {
    let code_index = &config.code_index;
    FeatureStatus::new(
        "code_index",
        "Code index",
        "Indexes repositories for semantic code search.",
        FeatureCategory::Intelligence,
    )
    .state(true, code_index.enabled)
    .setting("max_file_size_kb", code_index.max_file_size_kb)
    .setting("batch_size", code_index.batch_size)
    .setting("reindex_interval_hours", code_index.reindex_interval_hours)
}

fn retrieval_eval(config: &Config) -> FeatureStatus {
    FeatureStatus::new(
        "retrieval_eval",
        "Retrieval evaluation",
        "Grades how relevant the retrieved context was for each attempt.",
        FeatureCategory::Intelligence,
    )
    .state(true, config.retrieval_eval.enabled)
}

fn embedding(config: &Config) -> FeatureStatus {
    let embedding = &config.embedding;
    FeatureStatus::new(
        "embedding",
        "Embeddings",
        "The embedding model behind code and knowledge search.",
        FeatureCategory::Intelligence,
    )
    .setting("gpu", embedding.gpu)
    .setting("device_id", embedding.device_id)
    .setting("pool_size", embedding.pool_size)
    .setting("sub_batch_size", embedding.sub_batch_size)
}

fn llm(config: &Config) -> FeatureStatus {
    let llm = &config.llm;
    FeatureStatus::new(
        "llm",
        "Local LLM",
        "A local model for offline inference and chat.",
        FeatureCategory::Intelligence,
    )
    .state(true, llm.enabled)
    .setting("context_length", llm.context_length)
    .setting("gpu_layers", llm.gpu_layers)
    .setting("threads", llm.threads)
    .setting("inference_timeout_secs", llm.inference_timeout_secs)
    .setting("use_agent", llm.use_agent)
}

fn chat(config: &Config) -> FeatureStatus {
    let chat = &config.chat;
    FeatureStatus::new(
        "chat",
        "Code chat",
        "Chat about indexed code with the local model.",
        FeatureCategory::Intelligence,
    )
    .state(true, chat.enabled)
    .setting("temperature", chat.temperature)
    .setting("top_p", chat.top_p)
    .setting("max_tokens", chat.max_tokens)
    .setting("max_context_chunks", chat.max_context_chunks)
    .setting("max_history_messages", chat.max_history_messages)
    .setting("session_ttl_days", chat.session_ttl_days)
}

fn qa(config: &Config) -> FeatureStatus {
    let qa = &config.qa;
    FeatureStatus::new(
        "qa",
        "Question answering",
        "Answers questions grounded in indexed code and knowledge.",
        FeatureCategory::Intelligence,
    )
    .state(true, qa.enabled)
    .setting("max_context_chunks", qa.max_context_chunks)
    .setting("answer_timeout_secs", qa.answer_timeout_secs)
    .setting("max_qa_per_cycle", qa.max_qa_per_cycle)
    .setting("max_concurrent", qa.max_concurrent)
    .setting("use_llm", qa.use_llm)
}

fn discord_knowledgebase(config: &Config) -> FeatureStatus {
    let feature = FeatureStatus::new(
        "knowledgebase.discord",
        "Discord knowledge base",
        "Indexes Discord channels as searchable knowledge.",
        FeatureCategory::Intelligence,
    );
    let Some(knowledgebase) = config.knowledgebase.discord.as_ref() else {
        return feature.unconfigured();
    };
    feature
        .state(true, knowledgebase.enabled)
        .setting("guild_id", knowledgebase.guild_id.clone())
        .setting("categories", knowledgebase.categories.clone())
        .setting("ignore_channels", knowledgebase.ignore_channels.len())
        .setting("backfill_days", knowledgebase.backfill_days)
        .setting(
            "reindex_interval_hours",
            knowledgebase.reindex_interval_hours,
        )
        .secret("bot_token", knowledgebase.bot_token.as_ref())
}

fn dashboard(config: &Config) -> FeatureStatus {
    let dashboard = &config.dashboard;
    FeatureStatus::new(
        "dashboard",
        "Console",
        "Cost and time-saving figures shown in the console.",
        FeatureCategory::Platform,
    )
    .setting("max_plan_monthly_cost", dashboard.max_plan_monthly_cost)
    .setting("hourly_engineer_rate", dashboard.hourly_engineer_rate)
}

fn tls(config: &Config) -> FeatureStatus {
    let tls = &config.tls;
    FeatureStatus::new(
        "tls",
        "TLS",
        "Provisions HTTPS certificates through Let's Encrypt.",
        FeatureCategory::Platform,
    )
    .state(!tls.domains.is_empty(), tls.enabled)
    .setting("production", tls.production)
    .setting("https_port", tls.https_port)
    .setting("http_redirect_port", tls.http_redirect_port)
}

fn mcp_server(config: &Config) -> FeatureStatus {
    let server = &config.mcp_server;
    FeatureStatus::new(
        "mcp_server",
        "MCP search server",
        "Exposes code, Discord and HelpScout search as MCP tools.",
        FeatureCategory::Platform,
    )
    .state(true, server.enabled)
    .setting("expose_code", server.expose_code)
    .setting("expose_discord", server.expose_discord)
    .setting("expose_helpscout", server.expose_helpscout)
    .setting("default_limit", server.default_limit)
    .setting("max_limit", server.max_limit)
    .setting("require_auth", server.require_auth)
    .setting(
        "require_cloudflare_access",
        server.require_cloudflare_access,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::create_api_router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use claudear_config::config::{
        DeployQaTrackConfig, DiscordKnowledgebaseConfig, DiscordSourceConfig,
        GitHubCodeScanningConfig, GitLabConfig, HelpScoutConfig, JiraConfig, LinearConfig,
        McpServerConfig, SentryConfig, SlackSourceConfig, UserConfig,
    };
    use claudear_storage::{FixAttemptTracker, SqliteTracker};
    use http_body_util::BodyExt;
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tower::ServiceExt;
    use tower_cookies::CookieManagerLayer;

    /// Config sections with no feature entry yet. A new section must land in
    /// `BUILDERS` or here.
    const PENDING_UI: &[&str] = &["users"];

    const GROUPED_SECTIONS: &[&str] = &["issues", "knowledgebase", "notifiers", "scm"];

    fn populated_config() -> Config {
        let mut config = Config::default();
        config.issues.linear = Some(LinearConfig::default());
        config.issues.sentry = Some(SentryConfig::default());
        config.issues.jira = Some(JiraConfig::default());
        config.issues.discord = Some(DiscordSourceConfig::default());
        config.issues.slack = Some(SlackSourceConfig::default());
        config.issues.helpscout = Some(HelpScoutConfig::default());
        config.issues.github_code_scanning = Some(GitHubCodeScanningConfig::default());
        config.scm.gitlab = Some(GitLabConfig::default());
        config.knowledgebase.discord = Some(DiscordKnowledgebaseConfig::default());
        config
            .users
            .insert("someone".to_string(), UserConfig::default());
        config
    }

    fn leak(field: &str) -> String {
        format!("LEAKED-{field}")
    }

    fn fake(field: &str) -> Option<SecretValue> {
        Some(SecretValue::new(leak(field)))
    }

    fn config_with_fake_secrets() -> Config {
        let mut config = populated_config();

        let provider = config.agent.default_provider_config_mut();
        provider.api_key = fake("agent.api_key");
        provider
            .env
            .insert("ANTHROPIC_API_KEY".to_string(), leak("agent.env"));
        provider
            .extra
            .insert("token".to_string(), Value::String(leak("agent.extra")));
        provider.mcp.insert(
            "search".to_string(),
            McpServerConfig {
                env: [("API_KEY".to_string(), leak("mcp.env"))].into(),
                headers: [("Authorization".to_string(), leak("mcp.headers"))].into(),
                ..McpServerConfig::default()
            },
        );

        let github = &mut config.scm.github;
        github.token = fake("github.token");
        github.webhook_secret = fake("github.webhook_secret");
        github.app.private_key = fake("github.app.private_key");
        github.app.webhook_secret = fake("github.app.webhook_secret");
        github.app.client_secret = fake("github.app.client_secret");

        if let Some(gitlab) = config.scm.gitlab.as_mut() {
            gitlab.token = fake("gitlab.token");
            gitlab.webhook_secret = fake("gitlab.webhook_secret");
        }

        let issues = &mut config.issues;
        if let Some(linear) = issues.linear.as_mut() {
            linear.api_key = SecretValue::new(leak("linear.api_key"));
            linear.webhook_secret = fake("linear.webhook_secret");
        }
        if let Some(sentry) = issues.sentry.as_mut() {
            sentry.auth_token = SecretValue::new(leak("sentry.auth_token"));
            sentry.client_secret = fake("sentry.client_secret");
        }
        if let Some(jira) = issues.jira.as_mut() {
            jira.api_token = SecretValue::new(leak("jira.api_token"));
        }
        if let Some(discord) = issues.discord.as_mut() {
            discord.bot_token = fake("issues.discord.bot_token");
        }
        if let Some(slack) = issues.slack.as_mut() {
            slack.bot_token = fake("issues.slack.bot_token");
            slack.signing_secret = fake("issues.slack.signing_secret");
            slack.app_config_token = fake("issues.slack.app_config_token");
        }
        if let Some(helpscout) = issues.helpscout.as_mut() {
            helpscout.app_id = SecretValue::new(leak("helpscout.app_id"));
            helpscout.app_secret = SecretValue::new(leak("helpscout.app_secret"));
            helpscout.webhook_secret = fake("helpscout.webhook_secret");
        }

        let notifiers = &mut config.notifiers;
        notifiers.discord.webhook_url = fake("notifiers.discord.webhook_url");
        notifiers.discord.bot_token = fake("notifiers.discord.bot_token");
        notifiers.slack.webhook_url = fake("notifiers.slack.webhook_url");
        notifiers.slack.bot_token = fake("notifiers.slack.bot_token");
        notifiers.email.smtp_password = fake("email.smtp_password");
        notifiers.email.imap_password = fake("email.imap_password");
        notifiers.sms.account_sid = Some(leak("sms.account_sid"));
        notifiers.sms.auth_token = fake("sms.auth_token");
        notifiers.push.api_token = fake("push.api_token");
        notifiers.push.user_key = Some(leak("push.user_key"));
        notifiers.whatsapp.access_token = fake("whatsapp.access_token");
        notifiers.whatsapp.app_secret = fake("whatsapp.app_secret");
        notifiers.whatsapp.webhook_verify_token = fake("whatsapp.webhook_verify_token");
        notifiers.telegram.bot_token = fake("telegram.bot_token");

        config.regression.github_token = Some(leak("regression.github_token"));

        if let Some(knowledgebase) = config.knowledgebase.discord.as_mut() {
            knowledgebase.bot_token = fake("knowledgebase.discord.bot_token");
        }

        config
    }

    fn feature<'a>(features: &'a [FeatureStatus], key: &str) -> &'a FeatureStatus {
        features
            .iter()
            .find(|feature| feature.key == key)
            .unwrap_or_else(|| panic!("no feature {key}"))
    }

    fn expected_keys() -> BTreeSet<String> {
        let value = toml::Value::try_from(populated_config()).expect("config serialises");
        let table = value.as_table().expect("config is a table");
        let mut keys = BTreeSet::new();
        for (key, section) in table {
            let Some(section) = section.as_table() else {
                continue;
            };
            if GROUPED_SECTIONS.contains(&key.as_str()) {
                for (child, value) in section {
                    if value.is_table() {
                        keys.insert(format!("{key}.{child}"));
                    }
                }
            } else {
                keys.insert(key.clone());
            }
        }
        keys
    }

    fn covers(feature_key: &str, config_key: &str) -> bool {
        feature_key == config_key
            || feature_key
                .strip_prefix(config_key)
                .is_some_and(|rest| rest.starts_with('.'))
    }

    #[test]
    fn every_config_section_has_a_feature_or_is_pending() {
        let features = features(&populated_config());
        let expected = expected_keys();
        for group in GROUPED_SECTIONS {
            assert!(
                expected
                    .iter()
                    .any(|key| key.starts_with(&format!("{group}."))),
                "expected sub-sections of [{group}] in the populated config"
            );
        }
        let missing: Vec<&String> = expected
            .iter()
            .filter(|key| !PENDING_UI.contains(&key.as_str()))
            .filter(|key| !features.iter().any(|feature| covers(feature.key, key)))
            .collect();
        assert!(
            missing.is_empty(),
            "config sections without a feature entry or PENDING_UI entry: {missing:?}"
        );
        let unknown: Vec<&str> = features
            .iter()
            .map(|feature| feature.key)
            .filter(|key| !expected.iter().any(|expected| covers(key, expected)))
            .collect();
        assert!(
            unknown.is_empty(),
            "feature keys that are not config sections: {unknown:?}"
        );
    }

    #[test]
    fn feature_keys_are_unique() {
        let features = features(&Config::default());
        let keys: BTreeSet<&str> = features.iter().map(|feature| feature.key).collect();
        assert_eq!(keys.len(), features.len(), "duplicate feature keys");
    }

    #[test]
    fn enabled_implies_configured() {
        for config in [Config::default(), config_with_fake_secrets()] {
            for feature in features(&config) {
                assert!(
                    feature.configured || !feature.enabled,
                    "{} is enabled but not configured",
                    feature.key
                );
            }
        }
    }

    #[test]
    fn absent_optional_sections_are_not_configured() {
        let features = features(&Config::default());
        for key in [
            "issues.linear",
            "issues.sentry",
            "issues.jira",
            "issues.discord",
            "issues.slack",
            "issues.helpscout",
            "issues.github_code_scanning",
            "scm.gitlab",
            "knowledgebase.discord",
        ] {
            let feature = feature(&features, key);
            assert!(!feature.configured, "{key} should not be configured");
            assert!(!feature.enabled, "{key} should not be enabled");
        }
    }

    #[test]
    fn secret_values_are_never_reported() {
        let features = features(&config_with_fake_secrets());
        let body = serde_json::to_string(&FeaturesResponse { features }).unwrap();
        assert!(
            !body.contains("LEAKED-"),
            "feature status leaked a secret: {body}"
        );
    }

    #[test]
    fn secrets_report_whether_they_are_set() {
        let configured = features(&config_with_fake_secrets());
        let default = features(&Config::default());
        for (key, name) in [
            ("agent", "providers.claude.api_key"),
            ("scm.github", "token"),
            ("scm.github", "app.private_key"),
            ("notifiers.discord", "webhook_url"),
            ("notifiers.slack", "webhook_url"),
            ("issues.helpscout", "app_id"),
            ("regression", "github_token"),
            ("notifiers.sms", "account_sid"),
            ("notifiers.push", "user_key"),
        ] {
            let secret = |features: &[FeatureStatus]| {
                feature(features, key)
                    .secrets
                    .iter()
                    .find(|secret| secret.name == name)
                    .map(|secret| secret.set)
            };
            assert_eq!(
                secret(&configured),
                Some(true),
                "{key} {name} should be set"
            );
            if let Some(set) = secret(&default) {
                assert!(!set, "{key} {name} should not be set by default");
            }
        }
    }

    const INTERNAL_SETTINGS: &[&str] = &[
        "workspace",
        "base_url",
        "app.base_url",
        "app.private_key_path",
        "custom_jql",
        "smtp_host",
        "imap_host",
        "model_path",
        "domains",
        "allowed_origins",
    ];

    fn internal(field: &str) -> String {
        format!("INTERNAL-{field}")
    }

    fn config_with_internal_values() -> Config {
        let mut config = populated_config();
        config.workspace = PathBuf::from(internal("workspace"));
        config.scm.github.app.base_url = Some(internal("github.app.base_url"));
        config.scm.github.app.private_key_path = Some(PathBuf::from(internal("private_key_path")));
        if let Some(gitlab) = config.scm.gitlab.as_mut() {
            gitlab.base_url = internal("gitlab.base_url");
        }
        if let Some(jira) = config.issues.jira.as_mut() {
            jira.base_url = internal("jira.base_url");
            jira.custom_jql = Some(internal("jira.custom_jql"));
        }
        if let Some(slack) = config.issues.slack.as_mut() {
            slack.workspace = Some(internal("issues.slack.workspace"));
        }
        config.notifiers.slack.workspace = Some(internal("notifiers.slack.workspace"));
        config.notifiers.email.smtp_host = Some(internal("smtp_host"));
        config.notifiers.email.imap_host = Some(internal("imap_host"));
        config.llm.model_path = PathBuf::from(internal("model_path"));
        config.tls.domains = vec![internal("tls.domains")];
        config.mcp_server.allowed_origins = vec![internal("allowed_origins")];
        config
    }

    #[test]
    fn paths_hosts_and_queries_are_never_reported() {
        let features = features(&config_with_internal_values());
        for feature in &features {
            for setting in &feature.settings {
                assert!(
                    !INTERNAL_SETTINGS.contains(&setting.name.as_str()),
                    "{} reports internal setting {}",
                    feature.key,
                    setting.name
                );
            }
        }
        let body = serde_json::to_string(&FeaturesResponse { features }).unwrap();
        assert!(
            !body.contains("INTERNAL-"),
            "feature status leaked an internal value: {body}"
        );
    }

    #[test]
    fn deploy_qa_needs_tracks_to_be_configured() {
        let mut config = Config::default();
        config.deploy_qa.enabled = true;
        let status = |config: &Config| {
            let features = features(config);
            let feature = feature(&features, "deploy_qa");
            (feature.configured, feature.enabled)
        };
        assert_eq!(status(&config), (false, false));
        config.deploy_qa.tracks = vec![DeployQaTrackConfig {
            name: "cloud".to_string(),
            repo: "acme/cloud".to_string(),
            tag_filter: Default::default(),
        }];
        assert_eq!(status(&config), (true, true));
    }

    #[test]
    fn category_serialises_as_snake_case() {
        assert_eq!(
            serde_json::to_value(FeatureCategory::Intelligence).unwrap(),
            Value::String("intelligence".to_string())
        );
    }

    fn router(tracker: &Arc<dyn FixAttemptTracker>, storage: &std::path::Path) -> axum::Router {
        let mut config = config_with_fake_secrets();
        config.storage_dir = storage.to_path_buf();
        create_api_router(
            config,
            tracker.clone(),
            PathBuf::from("claudear.toml"),
            tracker.subscribe_indexing_progress(),
        )
        .layer(CookieManagerLayer::new())
    }

    #[tokio::test]
    async fn viewer_can_read_features() {
        let storage = tempfile::tempdir().unwrap();
        let tracker: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let password_hash = bcrypt::hash("testpass", 4).unwrap();
        let user_id = tracker
            .create_user("viewer@example.com", &password_hash, "Viewer", "viewer")
            .unwrap();
        let token = tracker
            .create_session(user_id, "2099-12-31 23:59:59")
            .unwrap();

        let response = router(&tracker, storage.path())
            .oneshot(
                Request::builder()
                    .uri("/api/features")
                    .header("cookie", format!("claudear_session={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(!text.contains("LEAKED-"), "response leaked a secret");
        let json: Value = serde_json::from_str(&text).unwrap();
        let features = json["features"].as_array().unwrap();
        assert_eq!(features.len(), BUILDERS.len());
        let linear = features
            .iter()
            .find(|feature| feature["key"] == "issues.linear")
            .unwrap();
        assert_eq!(linear["category"], "source");
        assert_eq!(linear["configured"], true);
        assert!(linear["settings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|setting| setting["name"] == "trigger_labels"));
        assert_eq!(
            linear["secrets"],
            serde_json::json!([
                { "name": "api_key", "set": true },
                { "name": "webhook_secret", "set": true },
            ])
        );
    }

    #[tokio::test]
    async fn unauthenticated_request_is_rejected() {
        let storage = tempfile::tempdir().unwrap();
        let tracker: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());

        let response = router(&tracker, storage.path())
            .oneshot(
                Request::builder()
                    .uri("/api/features")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}

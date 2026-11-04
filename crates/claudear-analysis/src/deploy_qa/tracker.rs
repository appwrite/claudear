//! Poll GitHub for new release tips and persist last-seen / attempt state.

use crate::deploy_qa::announcement::DeployQaAnnouncement;
use crate::deploy_qa::announcement::DeployQaAnnouncer;
use crate::deploy_qa::announcement::DeployRun;
use crate::deploy_qa::playbook::load_playbook;
use crate::deploy_qa::playbook::DEPLOY_QA_SOURCE;
use crate::deploy_qa::probe::VERDICT_ALL_VERIFIED;
use crate::deploy_qa::probe::VERDICT_FAIL;
use crate::deploy_qa::probe::VERDICT_PREFIX;
use crate::deploy_qa::probe::VERDICT_UNVERIFIED;
use crate::release::GitHubCommit;
use crate::release::GitHubRelease;
use crate::release::GitHubTag;
use crate::release::GitHubWorkflowRun;
use crate::release::GitIdentity;
use crate::release::ReleaseClient;
use abnegate_http::HttpClient;
use abnegate_http::ReqwestHttpClient;
use claudear_config::config::DeployQaConfig;
use claudear_config::config::DeployQaTagFilter;
use claudear_config::config::DeployQaTrackConfig;
use claudear_core::error::Result;
use claudear_core::types::DeployQaTip;
use claudear_core::types::DeployQaTipStatus;
use claudear_core::types::Issue;
use claudear_core::types::IssuePriority;
use claudear_core::types::IssueStatus;
use claudear_core::types::MatchPriority;
use claudear_core::types::MatchResult;
use claudear_storage::FixAttemptTracker;
use semver::Version;
use std::path::Path;
use std::sync::Arc;

/// Issue metadata key holding the `[[deploy_qa.tracks]]` name.
pub const TRACK_METADATA_KEY: &str = "deploy_qa_track";
/// Issue metadata key holding the GitHub `owner/repo`.
pub const REPO_METADATA_KEY: &str = "deploy_qa_repo";
/// Issue metadata key holding the release tag.
pub const TAG_METADATA_KEY: &str = "deploy_qa_tag";

/// A normalized tip the poller can persist and enqueue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseTip {
    /// GitHub `owner/repo`.
    pub repo: String,
    /// Tag name.
    pub tag: String,
    /// Release title.
    pub name: Option<String>,
    /// Release body.
    pub body: Option<String>,
    /// Published timestamp.
    pub published_at: Option<String>,
    /// HTML URL.
    pub html_url: String,
    /// GitHub login of the release author, or of whoever cut a bare tag.
    pub author_login: Option<String>,
    /// Set when the tip is a tag with no GitHub release.
    pub bare_tag: Option<BareTag>,
}

/// What a tip with no GitHub release keeps about its tag.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BareTag {
    /// The track's tag before this one.
    pub previous: Option<String>,
    /// The tagged commit, for a lightweight tag.
    pub commit_sha: Option<String>,
    /// The annotated tag object, for an annotated tag.
    pub annotation_sha: Option<String>,
}

/// What the poller learns about a bare tag before announcing it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct BareTagDetails {
    releaser_login: Option<String>,
    deploy_run: Option<DeployRun>,
}

/// Accounts that push and merge for people rather than being one.
const AUTOMATION_LOGINS: [&str; 1] = ["web-flow"];

const BOT_LOGIN_SUFFIX: &str = "[bot]";

const COMMIT_OBJECT_KIND: &str = "commit";

impl ReleaseTip {
    fn from_release(repo: &str, release: GitHubRelease) -> Self {
        Self {
            repo: repo.to_string(),
            tag: release.tag_name,
            name: release.name,
            body: release.body,
            published_at: release.published_at,
            html_url: release.html_url,
            author_login: release.author.map(|author| author.login),
            bare_tag: None,
        }
    }

    /// A tip for a bare tag. With no release notes to read, its body points
    /// the agent at the changes since `previous`, the track's tag before it.
    fn from_tag(repo: &str, tag: GitHubTag, previous: Option<&str>) -> Self {
        let body = previous.map(|previous| {
            format!(
                "No GitHub release: `{tag}` is a bare tag. Changes since `{previous}`: \
                 https://github.com/{repo}/compare/{previous}...{tag}",
                tag = tag.name,
            )
        });
        let bare_tag = BareTag {
            previous: previous.map(str::to_string),
            commit_sha: tag.commit.and_then(|commit| commit.sha),
            annotation_sha: tag.annotation,
        };
        Self {
            repo: repo.to_string(),
            html_url: format!("https://github.com/{repo}/releases/tag/{}", tag.name),
            name: Some(tag.name.clone()),
            tag: tag.name,
            body,
            published_at: None,
            author_login: None,
            bare_tag: Some(bare_tag),
        }
    }
}

/// Whether `login` is a person rather than GitHub's merge account or a bot.
fn is_person(login: &str) -> bool {
    !login.ends_with(BOT_LOGIN_SUFFIX)
        && !AUTOMATION_LOGINS
            .iter()
            .any(|automation| automation.eq_ignore_ascii_case(login))
}

/// The login on `commit` whose git identity has `email`.
fn login_with_email(commit: &GitHubCommit, email: &str) -> Option<String> {
    let has_email = |identity: &Option<GitIdentity>| {
        identity
            .as_ref()
            .and_then(|identity| identity.email.as_deref())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(email))
    };
    [
        (&commit.commit.author, &commit.author),
        (&commit.commit.committer, &commit.committer),
    ]
    .into_iter()
    .find(|(identity, account)| has_email(identity) && account.is_some())
    .and_then(|(_, account)| account.as_ref().map(|account| account.login.clone()))
}

/// Who cut a bare tag: the tagger of an annotated tag, else the tagged
/// commit's author, else the account whose push started its workflow run.
///
/// Git records a tagger's name and email but no login, so the tagger counts
/// only when the tagged commit carries that email on a GitHub account.
fn releaser_login(
    tagger: Option<&GitIdentity>,
    commit: Option<&GitHubCommit>,
    run: Option<&GitHubWorkflowRun>,
) -> Option<String> {
    let tagger = tagger
        .and_then(|tagger| tagger.email.as_deref())
        .zip(commit)
        .and_then(|(email, commit)| login_with_email(commit, email));
    let author =
        commit.and_then(|commit| commit.author.as_ref().map(|author| author.login.clone()));
    let actor = run.and_then(|run| run.actor.as_ref().map(|actor| actor.login.clone()));
    [tagger, author, actor]
        .into_iter()
        .flatten()
        .find(|login| is_person(login))
}

/// A tip already stored with no announcement still needs one until its QA
/// run has a verdict.
fn needs_announcement(stored: &DeployQaTip) -> bool {
    stored.discord_message_id.is_none()
        && matches!(
            stored.status,
            DeployQaTipStatus::Pending | DeployQaTipStatus::Running | DeployQaTipStatus::Errored
        )
}

/// The semantic version `tag` names on a track filtered by `filter`: the
/// [`DeployQaTagFilter::version`] part, with any leading `v`.
fn tag_version(filter: &DeployQaTagFilter, tag: &str) -> Option<Version> {
    let version = filter.version(tag);
    Version::parse(version.strip_prefix('v').unwrap_or(version)).ok()
}

/// The newest tag matching `filter` and the one before it, by semantic
/// version. `git/matching-refs` lists by name, which puts `1.2.99` above
/// `1.2.141`.
///
/// When no matching tag is a semantic version, the first match in listing
/// order is newest and has no predecessor.
fn newest_tags(
    filter: &DeployQaTagFilter,
    tags: Vec<GitHubTag>,
) -> Option<(GitHubTag, Option<GitHubTag>)> {
    let matching: Vec<GitHubTag> = tags
        .into_iter()
        .filter(|tag| filter.matches(&tag.name))
        .collect();
    let mut versioned: Vec<(Version, GitHubTag)> = matching
        .iter()
        .filter_map(|tag| tag_version(filter, &tag.name).map(|version| (version, tag.clone())))
        .collect();
    if versioned.is_empty() {
        return matching.into_iter().next().map(|tag| (tag, None));
    }
    versioned.sort_by(|(left, _), (right, _)| right.cmp(left));
    let mut ordered = versioned.into_iter().map(|(_, tag)| tag);
    let newest = ordered.next()?;
    Some((newest, ordered.next()))
}

/// What the poller did for one track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployQaPollAction {
    /// New tip persisted as pending and ready to enqueue.
    Enqueued,
    /// Same tip already recorded (no re-fire).
    SkippedDuplicate,
    /// Newer tip seen but a previous attempt is still pending/running.
    SkippedPreviousRunning,
    /// No matching published tip on this track.
    NoMatchingTip,
    /// Polling this track failed; carries the error.
    Errored(String),
}

/// Per-track poll result.
#[derive(Debug, Clone)]
pub struct DeployQaPollResult {
    /// Track name.
    pub track: String,
    /// Action taken.
    pub action: DeployQaPollAction,
    /// Tip under consideration, when one existed.
    pub tip: Option<ReleaseTip>,
}

/// Background poller for `[deploy_qa]` tracks.
pub struct DeployQaTracker<C: HttpClient = ReqwestHttpClient> {
    client: ReleaseClient<C>,
    store: Arc<dyn FixAttemptTracker>,
    config: DeployQaConfig,
    announcer: Option<Arc<dyn DeployQaAnnouncer>>,
}

impl DeployQaTracker<ReqwestHttpClient> {
    /// Create a poller with the default HTTP client.
    ///
    /// Fails when the configured playbook cannot be loaded, since the
    /// `deploy_qa` source could never run the tips this poller would persist.
    pub fn new(
        token: impl Into<String>,
        store: Arc<dyn FixAttemptTracker>,
        config: DeployQaConfig,
    ) -> Result<Self> {
        load_configured_playbook(&config)?;
        Ok(Self {
            client: ReleaseClient::new(token),
            store,
            config,
            announcer: None,
        })
    }
}

impl<C: HttpClient> DeployQaTracker<C> {
    /// Create a poller with a custom HTTP client (tests).
    pub fn with_http_client(
        client: ReleaseClient<C>,
        store: Arc<dyn FixAttemptTracker>,
        config: DeployQaConfig,
    ) -> Self {
        Self {
            client,
            store,
            config,
            announcer: None,
        }
    }

    /// Announce new bare-tag tips through `announcer`, since no release feed
    /// does, and store each announcement's message id on its tip so the QA
    /// outcome replies under it.
    pub fn with_announcer(mut self, announcer: Arc<dyn DeployQaAnnouncer>) -> Self {
        self.announcer = Some(announcer);
        self
    }

    /// Poll every configured track once.
    ///
    /// A failing track yields [`DeployQaPollAction::Errored`] without aborting the others.
    pub async fn poll_once(&self) -> Vec<DeployQaPollResult> {
        let mut results = Vec::with_capacity(self.config.tracks.len());
        for track in &self.config.tracks {
            let result = match self.poll_track(track).await {
                Ok(result) => result,
                Err(error) => DeployQaPollResult {
                    track: track.name.clone(),
                    action: DeployQaPollAction::Errored(error.to_string()),
                    tip: None,
                },
            };
            results.push(result);
        }
        results
    }

    /// Poll a single track: detect tip, persist last-seen, skip duplicates / in-flight.
    pub async fn poll_track(&self, track: &DeployQaTrackConfig) -> Result<DeployQaPollResult> {
        let Some(tip) = self
            .latest_matching_tip(&track.repo, &track.tag_filter)
            .await?
        else {
            return Ok(DeployQaPollResult {
                track: track.name.clone(),
                action: DeployQaPollAction::NoMatchingTip,
                tip: None,
            });
        };

        if let Some(stored) = self.store.get_deploy_qa_tip(&track.name, &tip.tag)? {
            if let Some(bare_tag) = tip
                .bare_tag
                .as_ref()
                .filter(|_| self.announcer.is_some() && needs_announcement(&stored))
            {
                let details = self.bare_tag_details(&tip, bare_tag).await;
                self.announce(track, &tip, details, &stored).await;
            }
            return Ok(DeployQaPollResult {
                track: track.name.clone(),
                action: DeployQaPollAction::SkippedDuplicate,
                tip: Some(tip),
            });
        }

        if self.config.skip_if_previous_running
            && self.store.track_has_in_flight_deploy_qa(&track.name)?
        {
            return Ok(DeployQaPollResult {
                track: track.name.clone(),
                action: DeployQaPollAction::SkippedPreviousRunning,
                tip: Some(tip),
            });
        }

        let mut tip = tip;
        let details = match tip.bare_tag.as_ref() {
            Some(bare_tag) => Some(self.bare_tag_details(&tip, bare_tag).await),
            None => None,
        };
        if tip.author_login.is_none() {
            tip.author_login = details
                .as_ref()
                .and_then(|details| details.releaser_login.clone());
        }

        let mut row = DeployQaTip::new(&track.name, &tip.repo, &tip.tag);
        row.published_at = tip.published_at.clone();
        row.html_url = Some(tip.html_url.clone());
        row.author_login = tip.author_login.clone();
        row.release_body = tip.body.clone();
        row.status = DeployQaTipStatus::Pending;
        let stored = self.store.upsert_deploy_qa_tip(&row)?;

        if stored.status != DeployQaTipStatus::Pending || stored.id == 0 {
            return Ok(DeployQaPollResult {
                track: track.name.clone(),
                action: DeployQaPollAction::SkippedDuplicate,
                tip: Some(tip),
            });
        }

        if let Some(details) = details {
            self.announce(track, &tip, details, &stored).await;
        }

        Ok(DeployQaPollResult {
            track: track.name.clone(),
            action: DeployQaPollAction::Enqueued,
            tip: Some(tip),
        })
    }

    /// Who cut `tip`'s bare tag and the workflow run its push started. A
    /// lookup that fails only leaves its part unknown.
    async fn bare_tag_details(&self, tip: &ReleaseTip, bare_tag: &BareTag) -> BareTagDetails {
        let repo = tip.repo.as_str();
        let annotated = match bare_tag.annotation_sha.as_deref() {
            Some(sha) => self
                .client
                .get_annotated_tag(repo, sha)
                .await
                .unwrap_or_else(|error| {
                    tracing::warn!(repo, tag = %tip.tag, %error, "deploy_qa: could not read the annotated tag");
                    None
                }),
            None => None,
        };
        let commit_sha = bare_tag.commit_sha.clone().or_else(|| {
            annotated
                .as_ref()
                .filter(|annotated| annotated.object.kind == COMMIT_OBJECT_KIND)
                .map(|annotated| annotated.object.sha.clone())
        });
        let commit = match commit_sha.as_deref() {
            Some(sha) => self.client.get_commit(repo, sha).await.unwrap_or_else(|error| {
                tracing::warn!(repo, tag = %tip.tag, %error, "deploy_qa: could not read the tagged commit");
                None
            }),
            None => None,
        };
        let runs = self
            .client
            .get_push_workflow_runs(repo, &tip.tag)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(repo, tag = %tip.tag, %error, "deploy_qa: could not list the tag's workflow runs");
                Vec::new()
            });
        let run = runs
            .iter()
            .find(|run| run.head_sha.is_some() && run.head_sha == commit_sha)
            .or_else(|| runs.first());
        BareTagDetails {
            releaser_login: releaser_login(
                annotated
                    .as_ref()
                    .and_then(|annotated| annotated.tagger.as_ref()),
                commit.as_ref(),
                run,
            ),
            deploy_run: run.map(|run| DeployRun {
                name: match (run.name.as_deref(), run.run_number) {
                    (Some(name), Some(number)) => format!("{name} #{number}"),
                    (Some(name), None) => name.to_string(),
                    (None, _) => "Workflow run".to_string(),
                },
                url: run.html_url.clone(),
            }),
        }
    }

    /// Announce `tip` and store the announcement's id on `stored`. Failures
    /// are logged: the outcome reporter posts on its own without one.
    async fn announce(
        &self,
        track: &DeployQaTrackConfig,
        tip: &ReleaseTip,
        details: BareTagDetails,
        stored: &DeployQaTip,
    ) {
        let Some(announcer) = self.announcer.as_ref() else {
            return;
        };
        let announcement = DeployQaAnnouncement {
            track: track.name.clone(),
            repo: tip.repo.clone(),
            tag: tip.tag.clone(),
            version: track.tag_filter.version(&tip.tag).to_string(),
            tag_url: stored
                .html_url
                .clone()
                .unwrap_or_else(|| tip.html_url.clone()),
            previous_tag: tip
                .bare_tag
                .as_ref()
                .and_then(|bare_tag| bare_tag.previous.clone()),
            deploy_run: details.deploy_run,
            releaser_login: stored.author_login.clone().or(details.releaser_login),
        };
        let message_id = match announcer.announce(&announcement).await {
            Ok(message_id) => message_id,
            Err(error) => {
                tracing::warn!(track = %track.name, tag = %tip.tag, %error, "deploy_qa: could not announce the bare tag");
                return;
            }
        };
        if let Err(error) =
            self.store
                .update_deploy_qa_discord_ids(stored.id, Some(&message_id), None)
        {
            tracing::warn!(track = %track.name, tag = %tip.tag, %error, "deploy_qa: could not record the announcement id");
        }
    }

    /// Newest non-draft release, or bare tag, matching `filter`.
    ///
    /// A [`DeployQaTagFilter::Prefix`] track reads the tags under its prefix,
    /// since a monorepo app may tag deploys that have no release while the
    /// repo's releases belong to another app. Other tracks consult tags only
    /// when the repo has no published releases at all; otherwise a matching
    /// tag may belong to an unpublished draft.
    async fn latest_matching_tip(
        &self,
        repo: &str,
        filter: &DeployQaTagFilter,
    ) -> Result<Option<ReleaseTip>> {
        const PAGE_SIZE: u32 = 30;

        if let DeployQaTagFilter::Prefix(prefix) = filter {
            let tags = self.client.get_tags_with_prefix(repo, prefix).await?;
            return self.latest_prefixed_tip(repo, filter, tags).await;
        }

        let published: Vec<GitHubRelease> = self
            .client
            .get_releases(repo, PAGE_SIZE)
            .await?
            .into_iter()
            .filter(|release| !release.draft)
            .collect();

        if !published.is_empty() {
            return Ok(published
                .into_iter()
                .find(|release| filter.matches(&release.tag_name))
                .map(|release| ReleaseTip::from_release(repo, release)));
        }

        let tags = self.client.get_tags_with_prefix(repo, "").await?;
        Ok(newest_tags(filter, tags).map(|(newest, previous)| {
            ReleaseTip::from_tag(repo, newest, previous.as_ref().map(|tag| tag.name.as_str()))
        }))
    }

    /// The newest of a prefixed track's `tags`, as its published release when
    /// it has one and as a bare tag otherwise. A prefixed tag that is not a
    /// semantic version is never a tip: it cannot be ordered.
    async fn latest_prefixed_tip(
        &self,
        repo: &str,
        filter: &DeployQaTagFilter,
        tags: Vec<GitHubTag>,
    ) -> Result<Option<ReleaseTip>> {
        let versioned = tags
            .into_iter()
            .filter(|tag| tag_version(filter, &tag.name).is_some())
            .collect();
        let Some((newest, previous)) = newest_tags(filter, versioned) else {
            return Ok(None);
        };
        if let Some(release) = self
            .client
            .get_release_by_tag(repo, &newest.name)
            .await?
            .filter(|release| !release.draft)
        {
            return Ok(Some(ReleaseTip::from_release(repo, release)));
        }
        Ok(Some(ReleaseTip::from_tag(
            repo,
            newest,
            previous.as_ref().map(|tag| tag.name.as_str()),
        )))
    }
}

fn load_configured_playbook(config: &DeployQaConfig) -> Result<String> {
    let path = config.instructions_path.as_ref().map(Path::new);
    load_playbook(path)
}

/// Build the synthetic live-QA issue for a new tip.
///
/// Discord routing is deliberately absent from the metadata: the deploy_qa
/// reporter posts to its configured `#releases` channel itself.
pub fn build_deploy_qa_issue(
    track: &DeployQaTrackConfig,
    tip: &ReleaseTip,
    playbook: &str,
) -> Issue {
    let issue_id = DeployQaTip::issue_id_for(&track.name, &tip.repo, &tip.tag);
    let version = track.tag_filter.version(&tip.tag);
    let title = format!(
        "[deploy_qa] {} {} — live QA (report only, no fixes)",
        track.name, tip.tag
    );
    let body = tip.body.as_deref().unwrap_or("(no release body)");
    let description = format!(
        "{playbook}\n\n---\n\n# Release tip\n\n\
         - Track: {track}\n\
         - Repo: {repo}\n\
         - Tag: {tag}\n\
         - Version: {version}\n\
         - URL: {url}\n\
         - Published: {published}\n\
         - Author: {author}\n\n\
         ## Release body\n\n{body}\n\n\
         ## Agent constraints\n\n\
         - Source is `{source}` — live QA: actually run the playbook's checks \
         against the shipped release and report what you observed.\n\
         - Make state-changing requests (create, update, delete) only in the \
         playbook's throwaway QA project, never in any other project or production data.\n\
         - Do **not** open a fix PR, branch, or commit for this announcement.\n\
         - Do **not** post to Discord; Claudear posts the outcome from this report.\n\
         - End with `{prefix} {all_verified}` (every LIVE-TESTABLE PR passed), \
         `{prefix} {unverified}` (nothing failed but a check was blocked), \
         or `{prefix} {fail}` (a LIVE-TESTABLE PR failed).\n",
        playbook = playbook.trim(),
        track = track.name,
        repo = tip.repo,
        tag = tip.tag,
        url = tip.html_url,
        published = tip.published_at.as_deref().unwrap_or("unknown"),
        author = tip.author_login.as_deref().unwrap_or("unknown"),
        source = DEPLOY_QA_SOURCE,
        prefix = VERDICT_PREFIX,
        all_verified = VERDICT_ALL_VERIFIED,
        unverified = VERDICT_UNVERIFIED,
        fail = VERDICT_FAIL,
    );

    let mut issue = Issue::new(
        issue_id,
        format!("{}-{version}", track.name),
        title,
        tip.html_url.clone(),
        DEPLOY_QA_SOURCE,
    );
    issue.description = Some(description);
    issue.priority = IssuePriority::High;
    issue.status = IssueStatus::Open;
    issue.set_metadata("routing_intent", "QA");
    issue.set_metadata(TRACK_METADATA_KEY, track.name.clone());
    issue.set_metadata(REPO_METADATA_KEY, tip.repo.clone());
    issue.set_metadata(TAG_METADATA_KEY, tip.tag.clone());
    issue
}

/// Criteria result used when the watcher picks up a pending tip.
pub fn deploy_qa_match_result(track: &str, tag: &str) -> MatchResult {
    MatchResult::matched(
        format!("new deploy_qa tip {track} {tag}"),
        MatchPriority::High,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deploy_qa::playbook::bundled_playbook;
    use crate::deploy_qa::probe::classify_deploy_qa_verdict;
    use crate::deploy_qa::probe::DeployQaVerdict;
    use abnegate_http::HttpClient;
    use abnegate_http::HttpResponse;
    use async_trait::async_trait;
    use claudear_storage::SqliteTracker;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct MapMockHttp {
        by_url: Mutex<HashMap<String, HttpResponse>>,
        default: HttpResponse,
    }

    impl MapMockHttp {
        fn new() -> Self {
            Self {
                by_url: Mutex::new(HashMap::new()),
                default: HttpResponse::new(404, r#"{"message":"Not Found"}"#),
            }
        }

        fn on(self, url: &str, status: u16, body: &str) -> Self {
            self.by_url
                .lock()
                .unwrap()
                .insert(url.to_string(), HttpResponse::new(status, body.to_string()));
            self
        }
    }

    #[async_trait]
    impl HttpClient for MapMockHttp {
        async fn get(
            &self,
            url: &str,
            _headers: Vec<(&str, String)>,
        ) -> abnegate_http::Result<HttpResponse> {
            let map = self.by_url.lock().unwrap();
            if let Some(response) = map.get(url) {
                return Ok(response.clone());
            }
            for (key, response) in map.iter() {
                if url.starts_with(key) {
                    return Ok(response.clone());
                }
            }
            Ok(self.default.clone())
        }
    }

    fn release_json(tag: &str, extra: &str) -> String {
        format!(
            r#"{{
                "id": 1,
                "tag_name": "{tag}",
                "name": "{tag}",
                "draft": false,
                "prerelease": false,
                "created_at": "2026-09-23T00:00:00Z",
                "published_at": "2026-09-23T01:00:00Z",
                "target_commitish": "main",
                "body": "Adds #42 live route",
                "html_url": "https://github.com/appwrite-labs/edge/releases/tag/{tag}",
                "author": {{ "login": "abnegate" }}
                {extra}
            }}"#
        )
    }

    fn track(name: &str, repo: &str, filter: DeployQaTagFilter) -> DeployQaTrackConfig {
        DeployQaTrackConfig {
            name: name.to_string(),
            repo: repo.to_string(),
            tag_filter: filter,
        }
    }

    fn config(tracks: Vec<DeployQaTrackConfig>, skip_if_running: bool) -> DeployQaConfig {
        DeployQaConfig {
            enabled: true,
            skip_if_previous_running: skip_if_running,
            tracks,
            ..DeployQaConfig::default()
        }
    }

    fn poller(
        http: MapMockHttp,
        store: Arc<dyn FixAttemptTracker>,
        config: DeployQaConfig,
    ) -> DeployQaTracker<MapMockHttp> {
        let client = ReleaseClient::with_http_client("test-token", http);
        DeployQaTracker::with_http_client(client, store, config)
    }

    #[test]
    fn unreadable_playbook_stops_the_poller() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let directory = tempfile::tempdir().unwrap();
        let config = DeployQaConfig {
            instructions_path: Some(directory.path().join("missing.md").display().to_string()),
            ..config(
                vec![track(
                    "cloud",
                    "appwrite-labs/cloud",
                    DeployQaTagFilter::Any,
                )],
                true,
            )
        };

        assert!(
            DeployQaTracker::new("test-token", store, config).is_err(),
            "a poller must not persist tips that the deploy_qa source cannot run"
        );
    }

    #[test]
    fn tag_filters_select_edge_tracks() {
        let database = DeployQaTagFilter::Suffix("-db".into());
        let network = DeployQaTagFilter::NotSuffix("-db".into());
        assert!(database.matches("1.0.0-db"));
        assert!(!database.matches("1.0.0"));
        assert!(network.matches("1.0.0"));
        assert!(!network.matches("1.0.0-db"));
    }

    fn release_tip(repo: &str, tag: &str, body: &str) -> ReleaseTip {
        ReleaseTip {
            repo: repo.into(),
            tag: tag.into(),
            name: Some(tag.into()),
            body: Some(body.into()),
            published_at: Some("2026-09-23T01:00:00Z".into()),
            html_url: format!("https://github.com/{repo}/releases/tag/{tag}"),
            author_login: Some("abnegate".into()),
            bare_tag: None,
        }
    }

    #[test]
    fn issue_carries_the_release_body_under_an_id_stable_per_tip_and_distinct_per_track() {
        let repo = "appwrite-labs/edge";
        let database = track("edge-db", repo, DeployQaTagFilter::Any);
        let network = track("edge-network", repo, DeployQaTagFilter::Any);
        let tip = release_tip(repo, "1.2.3", "Adds #99");

        let issue = build_deploy_qa_issue(&database, &tip, bundled_playbook());
        let description = issue
            .description
            .as_deref()
            .expect("the issue should carry the prompt");
        assert!(
            description.contains(tip.body.as_deref().unwrap()),
            "the agent must get the release body it checks: {description}"
        );

        let again = build_deploy_qa_issue(&database, &tip, bundled_playbook());
        assert_eq!(
            issue.id, again.id,
            "the same tip on the same track must map to the same issue"
        );

        let other_track = build_deploy_qa_issue(&network, &tip, bundled_playbook());
        assert_ne!(
            issue.id, other_track.id,
            "tracks sharing a repo must not share an issue"
        );
    }

    #[test]
    fn issue_carries_no_discord_routing() {
        let repo = "appwrite-labs/edge";
        let issue = build_deploy_qa_issue(
            &track("edge-db", repo, DeployQaTagFilter::Any),
            &release_tip(repo, "1.2.3", "Adds #99"),
            bundled_playbook(),
        );

        for key in ["channel_id", "guild_id"] {
            assert_eq!(
                issue.get_metadata::<String>(key),
                None,
                "a deploy_qa issue must not carry Discord routing metadata ({key}): the \
                 notifier would post raw agent answers into #releases beside the curated report"
            );
        }
    }

    #[test]
    fn prompt_teaches_verdict_footers_the_classifier_understands() {
        let repo = "appwrite-labs/edge";
        for (name, playbook) in [("empty", ""), ("bundled", bundled_playbook())] {
            let issue = build_deploy_qa_issue(
                &track("edge-db", repo, DeployQaTagFilter::Any),
                &release_tip(repo, "1.2.3", "Adds #99"),
                playbook,
            );
            let description = issue.description.expect("issue should carry the prompt");

            let verdicts: Vec<DeployQaVerdict> = description
                .match_indices(VERDICT_PREFIX)
                .map(|(index, _)| {
                    description[index + VERDICT_PREFIX.len()..]
                        .trim_start()
                        .chars()
                        .take_while(|character| {
                            character.is_ascii_alphanumeric() || *character == '_'
                        })
                        .collect::<String>()
                })
                .filter(|token| !token.is_empty())
                .map(|token| classify_deploy_qa_verdict(&format!("{VERDICT_PREFIX} {token}")))
                .collect();

            for expected in [
                DeployQaVerdict::AllVerified,
                DeployQaVerdict::Unverified,
                DeployQaVerdict::Fail,
            ] {
                assert!(
                    verdicts.contains(&expected),
                    "the prompt with the {name} playbook must teach a footer the classifier \
                     reads as {expected:?}: {verdicts:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn last_seen_and_no_refire_on_same_tip() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let config = config(
            vec![track(
                "edge-db",
                "appwrite-labs/edge",
                DeployQaTagFilter::Suffix("-db".into()),
            )],
            true,
        );
        let body = format!("[{}]", release_json("1.2.3-db", ""));
        let http = MapMockHttp::new().on(
            "https://api.github.com/repos/appwrite-labs/edge/releases",
            200,
            &body,
        );
        let tracker = poller(http, store.clone(), config);

        let first = tracker.poll_once().await;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].action, DeployQaPollAction::Enqueued);

        let last = store
            .get_last_seen_deploy_qa_tip("edge-db")
            .unwrap()
            .unwrap();
        assert_eq!(last.tag, "1.2.3-db");
        assert_eq!(last.status, DeployQaTipStatus::Pending);

        let second = tracker.poll_once().await;
        assert_eq!(second[0].action, DeployQaPollAction::SkippedDuplicate);
        assert_eq!(
            store.list_pending_deploy_qa_tips().unwrap().len(),
            1,
            "a duplicate tip must not be queued again"
        );
    }

    #[tokio::test]
    async fn suffix_filter_ignores_non_matching_latest() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let config = config(
            vec![track(
                "edge-db",
                "appwrite-labs/edge",
                DeployQaTagFilter::Suffix("-db".into()),
            )],
            true,
        );
        let body = format!(
            "[{}, {}]",
            release_json("2.0.0", ""),
            release_json("1.9.0-db", "")
        );
        let http = MapMockHttp::new().on(
            "https://api.github.com/repos/appwrite-labs/edge/releases",
            200,
            &body,
        );
        let tracker = poller(http, store, config);
        let result = tracker.poll_once().await;
        assert_eq!(result[0].action, DeployQaPollAction::Enqueued);
        assert_eq!(result[0].tip.as_ref().unwrap().tag, "1.9.0-db");
    }

    #[tokio::test]
    async fn skip_if_previous_running_blocks_newer_tip() {
        let sqlite = SqliteTracker::in_memory().unwrap();
        let mut running = DeployQaTip::new("cloud", "appwrite-labs/cloud", "1.0.0");
        running.status = DeployQaTipStatus::Running;
        sqlite.upsert_deploy_qa_tip(&running).unwrap();

        let store: Arc<dyn FixAttemptTracker> = Arc::new(sqlite);
        let config = config(
            vec![track(
                "cloud",
                "appwrite-labs/cloud",
                DeployQaTagFilter::Any,
            )],
            true,
        );
        let body = format!("[{}]", release_json("1.1.0", ""));
        let http = MapMockHttp::new().on(
            "https://api.github.com/repos/appwrite-labs/cloud/releases",
            200,
            &body,
        );
        let tracker = poller(http, store.clone(), config);
        let result = tracker.poll_once().await;
        assert_eq!(result[0].action, DeployQaPollAction::SkippedPreviousRunning);
        assert!(store.get_deploy_qa_tip("cloud", "1.1.0").unwrap().is_none());
    }

    #[tokio::test]
    async fn tags_fallback_when_releases_empty() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let config = config(
            vec![track("website", "appwrite/website", DeployQaTagFilter::Any)],
            true,
        );
        let http = MapMockHttp::new()
            .on(
                "https://api.github.com/repos/appwrite/website/releases",
                200,
                "[]",
            )
            .on(
                "https://api.github.com/repos/appwrite/website/git/matching-refs/tags/",
                200,
                &references_json(&["v0.4.0"]),
            );
        let tracker = poller(http, store.clone(), config);
        let result = tracker.poll_once().await;
        assert_eq!(result[0].action, DeployQaPollAction::Enqueued);
        assert_eq!(result[0].tip.as_ref().unwrap().tag, "v0.4.0");
        assert_eq!(
            store
                .get_last_seen_deploy_qa_tip("website")
                .unwrap()
                .unwrap()
                .tag,
            "v0.4.0"
        );
    }

    #[tokio::test]
    async fn overlapping_tracks_on_same_repo_get_distinct_issue_ids() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let first = track("cloud-a", "appwrite-labs/cloud", DeployQaTagFilter::Any);
        let second = track("cloud-b", "appwrite-labs/cloud", DeployQaTagFilter::Any);
        let config = config(vec![first.clone(), second.clone()], true);
        let body = format!("[{}]", release_json("1.0.0", ""));
        let http = MapMockHttp::new().on(
            "https://api.github.com/repos/appwrite-labs/cloud/releases",
            200,
            &body,
        );
        let tracker = poller(http, store.clone(), config);

        let mut ids = Vec::new();
        for track in [&first, &second] {
            let result = tracker.poll_track(track).await.unwrap();
            assert_eq!(
                result.action,
                DeployQaPollAction::Enqueued,
                "{}",
                track.name
            );
            let tip = result.tip.expect("an enqueued track should carry its tip");
            let issue = build_deploy_qa_issue(track, &tip, bundled_playbook());
            let stored = store
                .get_deploy_qa_tip_by_issue_id(&issue.id)
                .unwrap()
                .expect("the enqueued tip should be stored under its issue id");
            assert_eq!(stored.track, track.name);
            ids.push(issue.id);
        }
        assert_ne!(ids[0], ids[1]);
    }

    #[tokio::test]
    async fn release_author_flows_to_tip_and_issue() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let edge_database = track(
            "edge-db",
            "appwrite-labs/edge",
            DeployQaTagFilter::Suffix("-db".into()),
        );
        let config = config(vec![edge_database.clone()], true);
        let body = format!("[{}]", release_json("1.2.3-db", ""));
        let http = MapMockHttp::new().on(
            "https://api.github.com/repos/appwrite-labs/edge/releases",
            200,
            &body,
        );
        let tracker = poller(http, store.clone(), config);

        let results = tracker.poll_once().await;

        let stored = store
            .get_deploy_qa_tip("edge-db", "1.2.3-db")
            .unwrap()
            .expect("tip should be persisted");
        assert_eq!(stored.author_login.as_deref(), Some("abnegate"));
        let tip = results[0]
            .tip
            .as_ref()
            .expect("the polled tip should be kept");
        let issue = build_deploy_qa_issue(&edge_database, tip, bundled_playbook());
        let description = issue.description.as_deref().unwrap_or_default();
        assert!(
            description.contains("Author: abnegate"),
            "the prompt should name the release author: {description}"
        );
    }

    #[tokio::test]
    async fn unverified_tip_does_not_block_newer_tip() {
        let sqlite = SqliteTracker::in_memory().unwrap();
        let previous = sqlite
            .upsert_deploy_qa_tip(&DeployQaTip::new("cloud", "appwrite-labs/cloud", "1.0.0"))
            .unwrap();
        sqlite
            .update_deploy_qa_tip_status(previous.id, DeployQaTipStatus::Unverified, None)
            .unwrap();

        let store: Arc<dyn FixAttemptTracker> = Arc::new(sqlite);
        let config = config(
            vec![track(
                "cloud",
                "appwrite-labs/cloud",
                DeployQaTagFilter::Any,
            )],
            true,
        );
        let body = format!("[{}]", release_json("1.1.0", ""));
        let http = MapMockHttp::new().on(
            "https://api.github.com/repos/appwrite-labs/cloud/releases",
            200,
            &body,
        );
        let tracker = poller(http, store, config);

        let result = tracker.poll_once().await;

        assert_eq!(result[0].action, DeployQaPollAction::Enqueued);
    }

    #[tokio::test]
    async fn track_error_is_isolated_and_reported() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let config = config(
            vec![
                track("a", "org/a", DeployQaTagFilter::Any),
                track("b", "org/b", DeployQaTagFilter::Any),
            ],
            true,
        );
        let body = format!("[{}]", release_json("1.0.0", ""));
        let http = MapMockHttp::new()
            .on(
                "https://api.github.com/repos/org/a/releases",
                401,
                r#"{"message":"Bad credentials"}"#,
            )
            .on("https://api.github.com/repos/org/b/releases", 200, &body);
        let tracker = poller(http, store.clone(), config);

        let results = tracker.poll_once().await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].track, "a");
        assert!(
            matches!(results[0].action, DeployQaPollAction::Errored(_)),
            "expected Errored, got {:?}",
            results[0].action
        );
        assert_eq!(results[1].track, "b");
        assert_eq!(results[1].action, DeployQaPollAction::Enqueued);
        assert!(store.get_deploy_qa_tip("b", "1.0.0").unwrap().is_some());
    }

    #[tokio::test]
    async fn no_tag_fallback_when_releases_exist_but_none_match() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let config = config(
            vec![track(
                "edge-db",
                "appwrite-labs/edge",
                DeployQaTagFilter::Suffix("-db".into()),
            )],
            true,
        );
        let body = format!("[{}]", release_json("2.0.0", ""));
        let http = MapMockHttp::new()
            .on(
                "https://api.github.com/repos/appwrite-labs/edge/releases",
                200,
                &body,
            )
            .on(
                "https://api.github.com/repos/appwrite-labs/edge/git/matching-refs/tags/",
                200,
                &references_json(&["1.0.0-db"]),
            );
        let tracker = poller(http, store.clone(), config);

        let results = tracker.poll_once().await;

        assert_eq!(results[0].action, DeployQaPollAction::NoMatchingTip);
        assert!(store
            .get_deploy_qa_tip("edge-db", "1.0.0-db")
            .unwrap()
            .is_none());
    }

    const CONSOLE_PREFIX: &str = "apps/console/";
    const MONOREPO: &str = "appwrite/appwrite";
    const MONOREPO_RELEASES: &str = "https://api.github.com/repos/appwrite/appwrite/releases";
    const CONSOLE_REFERENCES: &str =
        "https://api.github.com/repos/appwrite/appwrite/git/matching-refs/tags/apps/console/";

    fn console_track() -> DeployQaTrackConfig {
        track(
            "console",
            MONOREPO,
            DeployQaTagFilter::Prefix(CONSOLE_PREFIX.into()),
        )
    }

    fn console_release_url(tag: &str) -> String {
        format!("{MONOREPO_RELEASES}/tags/{tag}")
    }

    /// `git/matching-refs` output for `tags`, in the name order GitHub lists.
    fn references_json(tags: &[&str]) -> String {
        let references: Vec<String> = tags
            .iter()
            .map(|tag| {
                format!(r#"{{"ref":"refs/tags/{tag}","object":{{"sha":"abc","type":"commit"}}}}"#)
            })
            .collect();
        format!("[{}]", references.join(","))
    }

    fn console_references() -> String {
        references_json(&[
            "apps/console/1.2.140",
            "apps/console/1.2.141",
            "apps/console/1.2.95",
            "apps/console/1.2.99",
        ])
    }

    #[tokio::test]
    async fn prefix_track_picks_the_newest_bare_tag_by_version_not_by_name() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let console = console_track();
        let http = MapMockHttp::new().on(CONSOLE_REFERENCES, 200, &console_references());
        let tracker = poller(http, store.clone(), config(vec![console.clone()], true));

        let result = tracker.poll_track(&console).await.unwrap();

        assert_eq!(result.action, DeployQaPollAction::Enqueued);
        let tip = result.tip.expect("an enqueued track should carry its tip");
        assert_eq!(tip.tag, "apps/console/1.2.141");
        assert_eq!(
            tip.html_url,
            "https://github.com/appwrite/appwrite/releases/tag/apps/console/1.2.141"
        );
        let body = tip.body.as_deref().unwrap_or_default();
        assert!(
            body.contains(
                "https://github.com/appwrite/appwrite/compare/apps/console/1.2.140...apps/console/1.2.141"
            ),
            "a bare tag's prompt must point at the changes since the previous console tag: {body}"
        );

        let issue = build_deploy_qa_issue(&console, &tip, bundled_playbook());
        assert_eq!(issue.id, "console:appwrite/appwrite:apps/console/1.2.141");
        assert_eq!(issue.short_id, "console-1.2.141");
        let description = issue.description.as_deref().unwrap_or_default();
        assert!(description.contains("- Tag: apps/console/1.2.141"));
        assert!(
            description.contains("- Version: 1.2.141"),
            "the agent compares the deployed console against the version, not the tag: {description}"
        );
        let stored = store
            .get_deploy_qa_tip_by_issue_id(&issue.id)
            .unwrap()
            .expect("a slashed tag must store and resolve under its issue id");
        assert_eq!(stored.tag, "apps/console/1.2.141");
        assert_eq!(stored.track, "console");

        let again = tracker.poll_track(&console).await.unwrap();
        assert_eq!(again.action, DeployQaPollAction::SkippedDuplicate);
    }

    #[tokio::test]
    async fn prefix_track_uses_the_release_when_its_tag_has_one() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let console = console_track();
        let http = MapMockHttp::new()
            .on(CONSOLE_REFERENCES, 200, &console_references())
            .on(
                &console_release_url("apps/console/1.2.141"),
                200,
                &release_json("apps/console/1.2.141", ""),
            );
        let tracker = poller(http, store, config(vec![console.clone()], true));

        let tip = tracker
            .poll_track(&console)
            .await
            .unwrap()
            .tip
            .expect("the release should be the tip");

        assert_eq!(tip.tag, "apps/console/1.2.141");
        assert_eq!(tip.body.as_deref(), Some("Adds #42 live route"));
        assert_eq!(tip.author_login.as_deref(), Some("abnegate"));
    }

    #[tokio::test]
    async fn prefix_track_ignores_tags_that_name_no_version() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let console = console_track();
        let http = MapMockHttp::new().on(
            CONSOLE_REFERENCES,
            200,
            &references_json(&["apps/console/latest", "apps/console/next"]),
        );
        let tracker = poller(http, store, config(vec![console.clone()], true));

        let result = tracker.poll_track(&console).await.unwrap();

        assert_eq!(result.action, DeployQaPollAction::NoMatchingTip);
    }

    #[tokio::test]
    async fn console_and_server_tracks_on_the_monorepo_each_take_their_own_tags() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let console = console_track();
        let server = track("server", MONOREPO, DeployQaTagFilter::Any);
        let releases = format!(
            "[{}, {}, {}]",
            release_json("2.7.0", ""),
            release_json("1.9.10", ""),
            release_json("cl-1.9.0-5", "")
        );
        let http = MapMockHttp::new()
            .on(MONOREPO_RELEASES, 200, &releases)
            .on(
                &console_release_url("apps/console/1.2.141"),
                404,
                r#"{"message":"Not Found"}"#,
            )
            .on(CONSOLE_REFERENCES, 200, &console_references());
        let tracker = poller(
            http,
            store.clone(),
            config(vec![server.clone(), console.clone()], true),
        );

        let results = tracker.poll_once().await;

        let tags: Vec<(String, String)> = results
            .iter()
            .map(|result| {
                (
                    result.track.clone(),
                    result
                        .tip
                        .as_ref()
                        .map(|tip| tip.tag.clone())
                        .unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            tags,
            vec![
                ("server".to_string(), "2.7.0".to_string()),
                ("console".to_string(), "apps/console/1.2.141".to_string()),
            ]
        );
        assert!(results
            .iter()
            .all(|result| result.action == DeployQaPollAction::Enqueued));
        assert!(store
            .get_deploy_qa_tip("console", "2.7.0")
            .unwrap()
            .is_none());
        assert!(store
            .get_deploy_qa_tip("server", "apps/console/1.2.141")
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn edge_network_and_database_tracks_never_share_a_tip() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let network = track(
            "edge",
            "appwrite-labs/edge",
            DeployQaTagFilter::NotSuffix("-db".into()),
        );
        let database = track(
            "edge-db",
            "appwrite-labs/edge",
            DeployQaTagFilter::Suffix("-db".into()),
        );
        let releases = format!(
            "[{}, {}, {}, {}]",
            release_json("0.16.10-db", ""),
            release_json("0.10.61", ""),
            release_json("0.10.60", ""),
            release_json("0.16.9-db", "")
        );
        let http = MapMockHttp::new().on(
            "https://api.github.com/repos/appwrite-labs/edge/releases",
            200,
            &releases,
        );
        let tracker = poller(
            http,
            store.clone(),
            config(vec![network.clone(), database.clone()], true),
        );

        let results = tracker.poll_once().await;

        assert_eq!(results[0].tip.as_ref().unwrap().tag, "0.10.61");
        assert_eq!(results[1].tip.as_ref().unwrap().tag, "0.16.10-db");
        assert!(results
            .iter()
            .all(|result| result.action == DeployQaPollAction::Enqueued));
        assert!(store
            .get_deploy_qa_tip("edge", "0.16.10-db")
            .unwrap()
            .is_none());
        assert!(store
            .get_deploy_qa_tip("edge-db", "0.10.61")
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn tags_fallback_takes_the_highest_version_over_name_order() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let website = track("website", "appwrite/website", DeployQaTagFilter::Any);
        let http = MapMockHttp::new()
            .on(
                "https://api.github.com/repos/appwrite/website/releases",
                200,
                "[]",
            )
            .on(
                "https://api.github.com/repos/appwrite/website/git/matching-refs/tags/",
                200,
                &references_json(&["v0.1.0", "v0.10.0", "v0.9.0"]),
            );
        let tracker = poller(http, store, config(vec![website.clone()], true));

        let tip = tracker.poll_track(&website).await.unwrap().tip.unwrap();

        assert_eq!(tip.tag, "v0.10.0");
        assert!(tip
            .body
            .as_deref()
            .is_some_and(|body| body.contains("compare/v0.9.0...v0.10.0")));
    }

    #[tokio::test]
    async fn tags_fallback_reads_every_tag_not_just_the_first_page() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let website = track("website", "appwrite/website", DeployQaTagFilter::Any);
        let names: Vec<String> = (0..40)
            .map(|index| format!("archive-{index:02}"))
            .chain(["v1.0.0".to_string(), "v2.0.0".to_string()])
            .collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let http = MapMockHttp::new()
            .on(
                "https://api.github.com/repos/appwrite/website/releases",
                200,
                "[]",
            )
            .on(
                "https://api.github.com/repos/appwrite/website/git/matching-refs/tags/",
                200,
                &references_json(&names),
            );
        let tracker = poller(http, store, config(vec![website.clone()], true));

        let tip = tracker.poll_track(&website).await.unwrap().tip.unwrap();

        assert_eq!(tip.tag, "v2.0.0");
    }

    #[test]
    fn newest_tags_keeps_listing_order_when_no_tag_is_a_version() {
        let tags = vec![
            GitHubTag {
                name: "nightly".into(),
                commit: None,
                annotation: None,
            },
            GitHubTag {
                name: "stable".into(),
                commit: None,
                annotation: None,
            },
        ];

        let (newest, previous) = newest_tags(&DeployQaTagFilter::Any, tags).unwrap();

        assert_eq!(newest.name, "nightly");
        assert!(previous.is_none());
    }

    const CONSOLE_COMMIT: &str = "https://api.github.com/repos/appwrite/appwrite/commits/abc";
    const CONSOLE_RUNS: &str = "https://api.github.com/repos/appwrite/appwrite/actions/runs?event=push&branch=apps%2Fconsole%2F1.2.141&per_page=10";
    const CONSOLE_RUN_URL: &str = "https://github.com/appwrite/appwrite/actions/runs/41684208512";

    /// Records announcements and answers with `announcement-<n>`, failing the
    /// first `failures` calls.
    #[derive(Default)]
    struct RecordingAnnouncer {
        announcements: Mutex<Vec<DeployQaAnnouncement>>,
        failures: Mutex<usize>,
    }

    impl RecordingAnnouncer {
        fn failing_once() -> Self {
            Self {
                failures: Mutex::new(1),
                ..Self::default()
            }
        }

        fn announced(&self) -> Vec<DeployQaAnnouncement> {
            self.announcements.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl DeployQaAnnouncer for RecordingAnnouncer {
        async fn announce(&self, announcement: &DeployQaAnnouncement) -> Result<String> {
            let mut failures = self.failures.lock().unwrap();
            if *failures > 0 {
                *failures -= 1;
                return Err(claudear_core::error::Error::Other("Discord is down".into()));
            }
            let mut announcements = self.announcements.lock().unwrap();
            announcements.push(announcement.clone());
            Ok(format!("announcement-{}", announcements.len()))
        }
    }

    fn commit_json(
        author: &str,
        author_email: &str,
        committer: &str,
        committer_email: &str,
    ) -> String {
        format!(
            r#"{{
                "author": {{ "login": "{author}" }},
                "committer": {{ "login": "{committer}" }},
                "commit": {{
                    "author": {{ "name": "{author}", "email": "{author_email}" }},
                    "committer": {{ "name": "{committer}", "email": "{committer_email}" }}
                }}
            }}"#
        )
    }

    fn runs_json(actor: &str) -> String {
        format!(
            r#"{{"workflow_runs": [{{
                "name": "Console / Production",
                "run_number": 312,
                "head_sha": "abc",
                "html_url": "{CONSOLE_RUN_URL}",
                "actor": {{ "login": "{actor}" }}
            }}]}}"#
        )
    }

    fn console_http() -> MapMockHttp {
        MapMockHttp::new()
            .on(CONSOLE_REFERENCES, 200, &console_references())
            .on(
                CONSOLE_COMMIT,
                200,
                &commit_json(
                    "atharvadeosthale",
                    "atharva@appwrite.io",
                    "web-flow",
                    "noreply@github.com",
                ),
            )
            .on(CONSOLE_RUNS, 200, &runs_json("atharvadeosthale"))
    }

    fn announcing_poller(
        http: MapMockHttp,
        store: Arc<dyn FixAttemptTracker>,
        announcer: Arc<RecordingAnnouncer>,
    ) -> DeployQaTracker<MapMockHttp> {
        poller(http, store, config(vec![console_track()], true)).with_announcer(announcer)
    }

    #[tokio::test]
    async fn new_bare_tag_is_announced_once_with_its_releaser_and_deploy() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let announcer = Arc::new(RecordingAnnouncer::default());
        let tracker = announcing_poller(console_http(), store.clone(), announcer.clone());

        let first = tracker.poll_once().await;
        let second = tracker.poll_once().await;

        assert_eq!(first[0].action, DeployQaPollAction::Enqueued);
        assert_eq!(second[0].action, DeployQaPollAction::SkippedDuplicate);
        assert_eq!(
            announcer.announced(),
            vec![DeployQaAnnouncement {
                track: "console".into(),
                repo: MONOREPO.into(),
                tag: "apps/console/1.2.141".into(),
                version: "1.2.141".into(),
                tag_url: "https://github.com/appwrite/appwrite/releases/tag/apps/console/1.2.141"
                    .into(),
                previous_tag: Some("apps/console/1.2.140".into()),
                deploy_run: Some(DeployRun {
                    name: "Console / Production #312".into(),
                    url: CONSOLE_RUN_URL.into(),
                }),
                releaser_login: Some("atharvadeosthale".into()),
            }],
            "a new bare tag is announced exactly once"
        );
        let stored = store
            .get_deploy_qa_tip("console", "apps/console/1.2.141")
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.discord_message_id.as_deref(),
            Some("announcement-1"),
            "the outcome must reply under the announcement"
        );
        assert_eq!(
            stored.author_login.as_deref(),
            Some("atharvadeosthale"),
            "FAIL must be able to @ whoever cut the tag"
        );
    }

    #[tokio::test]
    async fn a_restarted_poller_does_not_announce_a_tip_again() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let before = Arc::new(RecordingAnnouncer::default());
        announcing_poller(console_http(), store.clone(), before.clone())
            .poll_once()
            .await;

        let after = Arc::new(RecordingAnnouncer::default());
        let results = announcing_poller(console_http(), store, after.clone())
            .poll_once()
            .await;

        assert_eq!(before.announced().len(), 1);
        assert_eq!(results[0].action, DeployQaPollAction::SkippedDuplicate);
        assert!(after.announced().is_empty(), "{:?}", after.announced());
    }

    #[tokio::test]
    async fn a_failed_announcement_is_retried_until_it_lands() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let announcer = Arc::new(RecordingAnnouncer::failing_once());
        let tracker = announcing_poller(console_http(), store.clone(), announcer.clone());

        tracker.poll_once().await;
        let unannounced = store
            .get_deploy_qa_tip("console", "apps/console/1.2.141")
            .unwrap()
            .unwrap();
        tracker.poll_once().await;
        tracker.poll_once().await;

        assert_eq!(unannounced.discord_message_id, None);
        assert_eq!(announcer.announced().len(), 1);
        assert_eq!(
            store
                .get_deploy_qa_tip("console", "apps/console/1.2.141")
                .unwrap()
                .unwrap()
                .discord_message_id
                .as_deref(),
            Some("announcement-1")
        );
    }

    #[tokio::test]
    async fn a_tip_with_a_verdict_is_never_announced_late() {
        let sqlite = SqliteTracker::in_memory().unwrap();
        let reported = sqlite
            .upsert_deploy_qa_tip(&DeployQaTip::new(
                "console",
                MONOREPO,
                "apps/console/1.2.141",
            ))
            .unwrap();
        sqlite
            .update_deploy_qa_tip_status(reported.id, DeployQaTipStatus::Verified, None)
            .unwrap();
        let store: Arc<dyn FixAttemptTracker> = Arc::new(sqlite);
        let announcer = Arc::new(RecordingAnnouncer::default());

        announcing_poller(console_http(), store, announcer.clone())
            .poll_once()
            .await;

        assert!(announcer.announced().is_empty());
    }

    #[tokio::test]
    async fn tips_with_a_release_are_never_announced() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let announcer = Arc::new(RecordingAnnouncer::default());
        let cloud = track("cloud", "appwrite-labs/cloud", DeployQaTagFilter::Any);
        let http = console_http()
            .on(
                &console_release_url("apps/console/1.2.141"),
                200,
                &release_json("apps/console/1.2.141", ""),
            )
            .on(
                "https://api.github.com/repos/appwrite-labs/cloud/releases",
                200,
                &format!("[{}]", release_json("1.53.30", "")),
            );
        let tracker = poller(
            http,
            store.clone(),
            config(vec![console_track(), cloud], true),
        )
        .with_announcer(announcer.clone());

        let results = tracker.poll_once().await;

        assert!(results
            .iter()
            .all(|result| result.action == DeployQaPollAction::Enqueued));
        assert!(
            announcer.announced().is_empty(),
            "GitHub's release feed announces released tips: {:?}",
            announcer.announced()
        );
    }

    #[tokio::test]
    async fn an_annotated_tag_is_released_by_its_tagger() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let announcer = Arc::new(RecordingAnnouncer::default());
        let references = r#"[
            {"ref":"refs/tags/apps/console/1.2.140","object":{"sha":"abc","type":"commit"}},
            {"ref":"refs/tags/apps/console/1.2.141","object":{"sha":"tag141","type":"tag"}}
        ]"#;
        let http = MapMockHttp::new()
            .on(CONSOLE_REFERENCES, 200, references)
            .on(
                "https://api.github.com/repos/appwrite/appwrite/git/tags/tag141",
                200,
                r#"{
                    "tagger": { "name": "Arnab", "email": "arnab@appwrite.io" },
                    "object": { "sha": "def", "type": "commit" }
                }"#,
            )
            .on(
                "https://api.github.com/repos/appwrite/appwrite/commits/def",
                200,
                &commit_json(
                    "author-login",
                    "author@appwrite.io",
                    "ArnabChatterjee20k",
                    "arnab@appwrite.io",
                ),
            )
            .on(CONSOLE_RUNS, 200, &runs_json("actor-login"));
        let tracker = announcing_poller(http, store, announcer.clone());

        tracker.poll_once().await;

        assert_eq!(
            announcer.announced()[0].releaser_login.as_deref(),
            Some("ArnabChatterjee20k")
        );
    }

    fn identity(email: &str) -> GitIdentity {
        GitIdentity {
            name: None,
            email: Some(email.to_string()),
        }
    }

    fn account(login: &str) -> Option<crate::release::ReleaseAuthor> {
        Some(crate::release::ReleaseAuthor {
            login: login.to_string(),
        })
    }

    fn commit(
        author: Option<&str>,
        author_email: &str,
        committer: Option<&str>,
        committer_email: &str,
    ) -> GitHubCommit {
        GitHubCommit {
            author: author.and_then(account),
            committer: committer.and_then(account),
            commit: crate::release::GitHubCommitDetail {
                author: Some(identity(author_email)),
                committer: Some(identity(committer_email)),
            },
        }
    }

    fn run(actor: &str) -> GitHubWorkflowRun {
        GitHubWorkflowRun {
            name: None,
            run_number: None,
            head_sha: None,
            html_url: CONSOLE_RUN_URL.into(),
            actor: account(actor),
        }
    }

    #[test]
    fn releaser_is_the_tagger_then_the_commit_author_then_the_run_actor() {
        let tagged = commit(Some("author"), "author@x.io", Some("tagger"), "Tagger@X.io");
        let tagger = identity("tagger@x.io");
        let stranger = identity("someone@x.io");
        let actor = run("actor");

        assert_eq!(
            releaser_login(Some(&tagger), Some(&tagged), Some(&actor)).as_deref(),
            Some("tagger"),
            "a tagger whose email the tagged commit carries on an account wins"
        );
        assert_eq!(
            releaser_login(Some(&stranger), Some(&tagged), Some(&actor)).as_deref(),
            Some("author"),
            "an unresolvable tagger falls back to the commit author"
        );
        assert_eq!(
            releaser_login(None, Some(&tagged), Some(&actor)).as_deref(),
            Some("author")
        );
        let unlinked = commit(None, "author@x.io", Some("web-flow"), "noreply@github.com");
        assert_eq!(
            releaser_login(None, Some(&unlinked), Some(&actor)).as_deref(),
            Some("actor"),
            "a commit with no author account falls back to the run actor"
        );
        let bot_authored = commit(Some("dependabot[bot]"), "bot@x.io", None, "");
        assert_eq!(
            releaser_login(None, Some(&bot_authored), Some(&run("web-flow"))),
            None,
            "bots and GitHub's merge account are never the releaser"
        );
        assert_eq!(releaser_login(None, None, None), None);
    }
}

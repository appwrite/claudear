//! Poll GitHub for new release tips and persist last-seen / attempt state.

use crate::deploy_qa::playbook::{load_playbook, DEPLOY_QA_SOURCE};
use crate::deploy_qa::probe::{VERDICT_ALL_VERIFIED, VERDICT_FAIL, VERDICT_PREFIX};
use crate::release::{GitHubRelease, GitHubTag, ReleaseClient};
use claudear_config::config::{DeployQaConfig, DeployQaTagFilter, DeployQaTrackConfig};
use claudear_core::error::Result;
use claudear_core::http::{HttpClient, ReqwestHttpClient};
use claudear_core::types::{
    DeployQaTip, DeployQaTipStatus, Issue, IssuePriority, IssueStatus, MatchPriority, MatchResult,
};
use claudear_storage::FixAttemptTracker;
use std::path::Path;
use std::sync::Arc;

/// Issue metadata key holding the `[[deploy_qa.tracks]]` name.
pub const TRACK_METADATA_KEY: &str = "deploy_qa_track";
/// Issue metadata key holding the GitHub `owner/repo`.
pub const REPO_METADATA_KEY: &str = "deploy_qa_repo";
/// Issue metadata key holding the release tag.
pub const TAG_METADATA_KEY: &str = "deploy_qa_tag";
/// Issue metadata flag marking the issue as observe/report only (no fix PR).
pub const OBSERVE_ONLY_METADATA_KEY: &str = "observe_only";

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
    /// GitHub login of the release author.
    pub author_login: Option<String>,
}

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
        }
    }

    fn from_tag(repo: &str, tag: GitHubTag) -> Self {
        Self {
            repo: repo.to_string(),
            html_url: format!("https://github.com/{repo}/releases/tag/{}", tag.name),
            name: Some(tag.name.clone()),
            tag: tag.name,
            body: None,
            published_at: None,
            author_login: None,
        }
    }
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
    /// Synthetic issue built for enqueue (only on [`DeployQaPollAction::Enqueued`]).
    pub issue: Option<Issue>,
}

/// Background poller for `[deploy_qa]` tracks.
pub struct DeployQaTracker<C: HttpClient = ReqwestHttpClient> {
    client: ReleaseClient<C>,
    store: Arc<dyn FixAttemptTracker>,
    config: DeployQaConfig,
    playbook: String,
}

impl DeployQaTracker<ReqwestHttpClient> {
    /// Create a poller with the default HTTP client.
    pub fn new(
        token: impl Into<String>,
        store: Arc<dyn FixAttemptTracker>,
        config: DeployQaConfig,
    ) -> Result<Self> {
        let playbook = load_configured_playbook(&config)?;
        Ok(Self {
            client: ReleaseClient::new(token),
            store,
            config,
            playbook,
        })
    }
}

impl<C: HttpClient> DeployQaTracker<C> {
    /// Create a poller with a custom HTTP client (tests).
    pub fn with_http_client(
        client: ReleaseClient<C>,
        store: Arc<dyn FixAttemptTracker>,
        config: DeployQaConfig,
        playbook: impl Into<String>,
    ) -> Self {
        Self {
            client,
            store,
            config,
            playbook: playbook.into(),
        }
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
                    issue: None,
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
                issue: None,
            });
        };

        if self
            .store
            .get_deploy_qa_tip(&track.name, &tip.tag)?
            .is_some()
        {
            return Ok(DeployQaPollResult {
                track: track.name.clone(),
                action: DeployQaPollAction::SkippedDuplicate,
                tip: Some(tip),
                issue: None,
            });
        }

        if self.config.skip_if_previous_running
            && self.store.track_has_in_flight_deploy_qa(&track.name)?
        {
            return Ok(DeployQaPollResult {
                track: track.name.clone(),
                action: DeployQaPollAction::SkippedPreviousRunning,
                tip: Some(tip),
                issue: None,
            });
        }

        let mut row = DeployQaTip::new(&track.name, &tip.repo, &tip.tag);
        row.published_at = tip.published_at.clone();
        row.html_url = Some(tip.html_url.clone());
        row.author_login = tip.author_login.clone();
        row.release_body = tip.body.clone();
        row.status = DeployQaTipStatus::Pending;
        let stored = self.store.upsert_deploy_qa_tip(&row)?;

        // upsert is insert-or-keep. If a concurrent poll already stored this
        // tag, treat it as a duplicate rather than re-enqueueing.
        if stored.status != DeployQaTipStatus::Pending || stored.id == 0 {
            return Ok(DeployQaPollResult {
                track: track.name.clone(),
                action: DeployQaPollAction::SkippedDuplicate,
                tip: Some(tip),
                issue: None,
            });
        }

        let issue = build_deploy_qa_issue(track, &tip, &self.playbook);
        Ok(DeployQaPollResult {
            track: track.name.clone(),
            action: DeployQaPollAction::Enqueued,
            tip: Some(tip),
            issue: Some(issue),
        })
    }

    /// Newest non-draft release matching `filter`.
    ///
    /// Tags are consulted only when the repo has no published releases at all;
    /// otherwise a matching tag may belong to an unpublished draft.
    async fn latest_matching_tip(
        &self,
        repo: &str,
        filter: &DeployQaTagFilter,
    ) -> Result<Option<ReleaseTip>> {
        const PAGE_SIZE: u32 = 30;

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

        Ok(self
            .client
            .get_tags(repo, PAGE_SIZE)
            .await?
            .into_iter()
            .find(|tag| filter.matches(&tag.name))
            .map(|tag| ReleaseTip::from_tag(repo, tag)))
    }
}

fn load_configured_playbook(config: &DeployQaConfig) -> Result<String> {
    let path = config.instructions_path.as_ref().map(Path::new);
    load_playbook(path)
}

/// Build the synthetic observe/report issue for a new tip.
///
/// Discord routing is deliberately absent from the metadata: the deploy_qa
/// reporter posts to its configured `#releases` channel itself.
pub fn build_deploy_qa_issue(
    track: &DeployQaTrackConfig,
    tip: &ReleaseTip,
    playbook: &str,
) -> Issue {
    let issue_id = DeployQaTip::issue_id_for(&track.name, &tip.repo, &tip.tag);
    let title = format!(
        "[deploy_qa] {} {} — live QA (observe/report only)",
        track.name, tip.tag
    );
    let body = tip.body.as_deref().unwrap_or("(no release body)");
    let description = format!(
        "{playbook}\n\n---\n\n# Release tip\n\n\
         - Track: {track}\n\
         - Repo: {repo}\n\
         - Tag: {tag}\n\
         - URL: {url}\n\
         - Published: {published}\n\
         - Author: {author}\n\n\
         ## Release body\n\n{body}\n\n\
         ## Agent constraints\n\n\
         - Source is `{source}` — observe, probe, and report only.\n\
         - Do **not** open a fix PR, branch, or commit for this announcement.\n\
         - End with `{prefix} {all_verified}` or `{prefix} {fail}`.\n",
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
        fail = VERDICT_FAIL,
    );

    let mut issue = Issue::new(
        issue_id,
        format!("{}-{}", track.name, tip.tag),
        title,
        tip.html_url.clone(),
        DEPLOY_QA_SOURCE,
    );
    issue.description = Some(description);
    issue.priority = IssuePriority::High;
    issue.status = IssueStatus::Open;
    issue.set_metadata("routing_intent", "QA");
    issue.set_metadata(OBSERVE_ONLY_METADATA_KEY, true);
    issue.set_metadata(TRACK_METADATA_KEY, track.name.clone());
    issue.set_metadata(REPO_METADATA_KEY, tip.repo.clone());
    issue.set_metadata(TAG_METADATA_KEY, tip.tag.clone());
    if let Some(ref author) = tip.author_login {
        issue.set_metadata("github_login", author.clone());
    }
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
    use crate::deploy_qa::probe::{classify_deploy_qa_verdict, DeployQaVerdict};
    use async_trait::async_trait;
    use claudear_core::http::{HttpClient, HttpResponse};
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
                default: HttpResponse {
                    status: 404,
                    body: r#"{"message":"Not Found"}"#.to_string(),
                },
            }
        }

        fn on(self, url: &str, status: u16, body: &str) -> Self {
            self.by_url.lock().unwrap().insert(
                url.to_string(),
                HttpResponse {
                    status,
                    body: body.to_string(),
                },
            );
            self
        }
    }

    #[async_trait]
    impl HttpClient for MapMockHttp {
        async fn get(&self, url: &str, _headers: Vec<(&str, String)>) -> Result<HttpResponse> {
            let map = self.by_url.lock().unwrap();
            if let Some(resp) = map.get(url) {
                return Ok(HttpResponse {
                    status: resp.status,
                    body: resp.body.clone(),
                });
            }
            // Prefix match so query strings still hit the stub.
            for (key, resp) in map.iter() {
                if url.starts_with(key) {
                    return Ok(HttpResponse {
                        status: resp.status,
                        body: resp.body.clone(),
                    });
                }
            }
            Ok(HttpResponse {
                status: self.default.status,
                body: self.default.body.clone(),
            })
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
        cfg: DeployQaConfig,
    ) -> DeployQaTracker<MapMockHttp> {
        let client = ReleaseClient::with_http_client("test-token", http);
        DeployQaTracker::with_http_client(client, store, cfg, bundled_playbook())
    }

    #[test]
    fn tag_filters_select_edge_tracks() {
        let db = DeployQaTagFilter::Suffix("-db".into());
        let net = DeployQaTagFilter::NotSuffix("-db".into());
        assert!(db.matches("1.0.0-db"));
        assert!(!db.matches("1.0.0"));
        assert!(net.matches("1.0.0"));
        assert!(!net.matches("1.0.0-db"));
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
        }
    }

    #[test]
    fn issue_is_observe_only_and_not_a_fix() {
        let repo = "appwrite-labs/edge";
        let db = track("edge-db", repo, DeployQaTagFilter::Any);
        let network = track("edge-network", repo, DeployQaTagFilter::Any);
        let tip = release_tip(repo, "1.2.3", "Adds #99");

        let issue = build_deploy_qa_issue(&db, &tip, bundled_playbook());
        assert_eq!(issue.source, DEPLOY_QA_SOURCE);
        assert_eq!(
            issue.get_metadata::<bool>(OBSERVE_ONLY_METADATA_KEY),
            Some(true)
        );
        assert!(issue
            .description
            .as_deref()
            .unwrap()
            .contains(tip.body.as_deref().unwrap()));

        let again = build_deploy_qa_issue(&db, &tip, bundled_playbook());
        assert_eq!(issue.id, again.id);

        let other_track = build_deploy_qa_issue(&network, &tip, bundled_playbook());
        assert_ne!(issue.id, other_track.id);
    }

    #[test]
    fn prompt_teaches_verdict_footers_the_classifier_understands() {
        let repo = "appwrite-labs/edge";
        let issue = build_deploy_qa_issue(
            &track("edge-db", repo, DeployQaTagFilter::Any),
            &release_tip(repo, "1.2.3", "Adds #99"),
            bundled_playbook(),
        );
        let description = issue.description.expect("issue should carry the prompt");

        let verdicts: Vec<DeployQaVerdict> = description
            .match_indices(VERDICT_PREFIX)
            .map(|(index, _)| {
                description[index + VERDICT_PREFIX.len()..]
                    .trim_start()
                    .chars()
                    .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
                    .collect::<String>()
            })
            .filter(|token| !token.is_empty())
            .map(|token| classify_deploy_qa_verdict(&format!("{VERDICT_PREFIX} {token}")))
            .collect();

        assert!(
            verdicts.contains(&DeployQaVerdict::AllVerified),
            "the prompt must teach a footer the classifier reads as all-verified: {verdicts:?}"
        );
        assert!(
            verdicts.contains(&DeployQaVerdict::Fail),
            "the prompt must teach a footer the classifier reads as a failure: {verdicts:?}"
        );
    }

    #[tokio::test]
    async fn last_seen_and_no_refire_on_same_tip() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let cfg = config(
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
        let tracker = poller(http, store.clone(), cfg);

        let first = tracker.poll_once().await;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].action, DeployQaPollAction::Enqueued);
        assert!(first[0].issue.is_some());

        let last = store
            .get_last_seen_deploy_qa_tip("edge-db")
            .unwrap()
            .unwrap();
        assert_eq!(last.tag, "1.2.3-db");
        assert_eq!(last.status, DeployQaTipStatus::Pending);

        let second = tracker.poll_once().await;
        assert_eq!(second[0].action, DeployQaPollAction::SkippedDuplicate);
        assert!(second[0].issue.is_none());
    }

    #[tokio::test]
    async fn suffix_filter_ignores_non_matching_latest() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let cfg = config(
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
        let tracker = poller(http, store, cfg);
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
        let cfg = config(
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
        let tracker = poller(http, store.clone(), cfg);
        let result = tracker.poll_once().await;
        assert_eq!(result[0].action, DeployQaPollAction::SkippedPreviousRunning);
        assert!(store.get_deploy_qa_tip("cloud", "1.1.0").unwrap().is_none());
    }

    #[tokio::test]
    async fn tags_fallback_when_releases_empty() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let cfg = config(
            vec![track("vibes", "appwrite/vibes", DeployQaTagFilter::Any)],
            true,
        );
        let http = MapMockHttp::new()
            .on(
                "https://api.github.com/repos/appwrite/vibes/releases",
                200,
                "[]",
            )
            .on(
                "https://api.github.com/repos/appwrite/vibes/tags",
                200,
                r#"[{"name":"v0.4.0","commit":{"sha":"abc"}}]"#,
            );
        let tracker = poller(http, store.clone(), cfg);
        let result = tracker.poll_once().await;
        assert_eq!(result[0].action, DeployQaPollAction::Enqueued);
        assert_eq!(result[0].tip.as_ref().unwrap().tag, "v0.4.0");
        assert_eq!(
            store
                .get_last_seen_deploy_qa_tip("vibes")
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
        let cfg = config(vec![first.clone(), second.clone()], true);
        let body = format!("[{}]", release_json("1.0.0", ""));
        let http = MapMockHttp::new().on(
            "https://api.github.com/repos/appwrite-labs/cloud/releases",
            200,
            &body,
        );
        let tracker = poller(http, store.clone(), cfg);

        let mut ids = Vec::new();
        for track in [&first, &second] {
            let result = tracker.poll_track(track).await.unwrap();
            assert_eq!(
                result.action,
                DeployQaPollAction::Enqueued,
                "{}",
                track.name
            );
            let issue = result.issue.unwrap();
            let stored = store
                .get_deploy_qa_tip_by_issue_id(&issue.id)
                .unwrap()
                .unwrap();
            assert_eq!(stored.track, track.name);
            ids.push(issue.id);
        }
        assert_ne!(ids[0], ids[1]);
    }

    #[tokio::test]
    async fn release_author_flows_to_tip_and_issue() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let cfg = config(
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
        let tracker = poller(http, store.clone(), cfg);

        let results = tracker.poll_once().await;

        let stored = store
            .get_deploy_qa_tip("edge-db", "1.2.3-db")
            .unwrap()
            .expect("tip should be persisted");
        assert_eq!(stored.author_login.as_deref(), Some("abnegate"));
        let issue = results[0].issue.as_ref().expect("issue should be built");
        assert_eq!(
            issue.get_metadata::<String>("github_login").as_deref(),
            Some("abnegate")
        );
    }

    #[tokio::test]
    async fn track_error_is_isolated_and_reported() {
        let store: Arc<dyn FixAttemptTracker> = Arc::new(SqliteTracker::in_memory().unwrap());
        let cfg = config(
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
        let tracker = poller(http, store.clone(), cfg);

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
        let cfg = config(
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
                "https://api.github.com/repos/appwrite-labs/edge/tags",
                200,
                r#"[{"name":"1.0.0-db","commit":{"sha":"abc"}}]"#,
            );
        let tracker = poller(http, store.clone(), cfg);

        let results = tracker.poll_once().await;

        assert_eq!(results[0].action, DeployQaPollAction::NoMatchingTip);
        assert!(store
            .get_deploy_qa_tip("edge-db", "1.0.0-db")
            .unwrap()
            .is_none());
    }
}

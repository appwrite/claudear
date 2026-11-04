//! Whether a merged commit is live in production, decided from the image tags
//! pinned in a deployment configuration repository rather than from the latest
//! GitHub release, and for packages, from the lock file at each pinned tag.

use super::github::ReleaseClient;
use abnegate_http::HttpClient;
use abnegate_http::ReqwestHttpClient;
use async_trait::async_trait;
use chrono::DateTime;
use chrono::Utc;
use claudear_core::error::Error;
use claudear_core::error::Result;
use futures::future::try_join_all;
use serde::Deserialize;

const DEFAULT_VALUES_FILE: &str = "default.yaml";
const CHART_FILE: &str = "Chart.yaml";
const IMAGE_KEY: &str = "image";
const REPOSITORY_KEY: &str = "repository";
const TAG_KEY: &str = "tag";
const LIST_ITEM_SEGMENT: &str = "[]";

/// The production deployment: which configuration repository pins the image
/// tags, and which source repositories each deployed image is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentTopology {
    pub configuration_repository: String,
    pub configuration_reference: String,
    pub applications: Vec<DeployedApplication>,
}

/// A source repository whose git tags are the `image` tags pinned in the
/// values files of `directories`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployedApplication {
    pub repository: String,
    pub image: String,
    pub directories: Vec<String>,
    pub packages: Vec<DeployedPackage>,
}

/// A repository that reaches an application through its `lock_file`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployedPackage {
    pub repository: String,
    pub package: String,
    pub lock_file: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveStatus {
    /// Every pinned production version contains the commit.
    Live { versions: Vec<String> },
    /// At least one pinned production version does not contain it yet.
    Pending { versions: Vec<String> },
    /// The repository is not part of the deployment topology.
    Untracked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryRole {
    Application,
    Package,
    Untracked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestState {
    pub merged: bool,
    pub closed: bool,
    pub merge_commit_sha: Option<String>,
    pub merged_at: Option<DateTime<Utc>>,
}

/// An `image:` mapping in a values file. `path` is its key path, such as
/// `global.image` or `cloud.initContainers[].image`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageReference {
    pub path: String,
    pub repository: Option<String>,
    pub tag: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedPackage {
    pub version: String,
    pub reference: Option<String>,
}

#[async_trait]
pub trait DeploymentStatus: Send + Sync {
    fn role(&self, repository: &str) -> RepositoryRole;
    async fn pull_request(&self, repository: &str, number: i64)
        -> Result<Option<PullRequestState>>;
    /// Any lookup or access failure is an error, never [`LiveStatus::Live`].
    async fn commit_status(&self, repository: &str, commit: &str) -> Result<LiveStatus>;
}

pub struct DeploymentResolver<C: HttpClient = ReqwestHttpClient> {
    client: ReleaseClient<C>,
    topology: DeploymentTopology,
}

impl DeploymentResolver<ReqwestHttpClient> {
    pub fn new(token: impl Into<String>, topology: DeploymentTopology) -> Self {
        Self::with_client(ReleaseClient::new(token), topology)
    }
}

impl<C: HttpClient> DeploymentResolver<C> {
    pub fn with_client(client: ReleaseClient<C>, topology: DeploymentTopology) -> Self {
        Self { client, topology }
    }

    /// The unique image tags of `application` pinned across its directories.
    /// A directory without a `default.yaml`, or one that pins no tag, is an
    /// error.
    pub async fn pinned_versions(&self, application: &DeployedApplication) -> Result<Vec<String>> {
        let mut versions = Vec::new();
        for directory in &application.directories {
            for tag in self.directory_versions(application, directory).await? {
                if !versions.contains(&tag) {
                    versions.push(tag);
                }
            }
        }
        Ok(versions)
    }

    async fn directory_versions(
        &self,
        application: &DeployedApplication,
        directory: &str,
    ) -> Result<Vec<String>> {
        let configuration = &self.topology.configuration_repository;
        let reference = &self.topology.configuration_reference;
        let files: Vec<String> = self
            .client
            .list_directory(configuration, directory, reference)
            .await?
            .into_iter()
            .filter(|path| is_values_file(path))
            .collect();
        let default_file = files
            .iter()
            .position(|path| file_name(path) == DEFAULT_VALUES_FILE)
            .ok_or_else(|| {
                Error::Other(format!(
                    "{configuration}/{directory}@{reference} has no {DEFAULT_VALUES_FILE}"
                ))
            })?;
        let contents = try_join_all(files.iter().map(|path| async move {
            self.client
                .get_file_at_ref(configuration, path, reference)
                .await?
                .ok_or_else(|| {
                    Error::Other(format!("{configuration}/{path}@{reference} disappeared"))
                })
        }))
        .await?;

        let references: Vec<Vec<ImageReference>> =
            contents.iter().map(|yaml| image_references(yaml)).collect();
        let default_paths: Vec<&str> = references[default_file]
            .iter()
            .filter(|reference| reference.repository.as_deref() == Some(application.image.as_str()))
            .map(|reference| reference.path.as_str())
            .collect();

        let mut versions = Vec::new();
        for reference in references.iter().flatten() {
            let pins_image = match reference.repository.as_deref() {
                Some(repository) => repository == application.image,
                None => default_paths.contains(&reference.path.as_str()),
            };
            let Some(tag) = reference.tag.as_deref().filter(|_| pins_image) else {
                continue;
            };
            let tag = validated_reference(tag)?;
            if !versions.iter().any(|version| version == tag) {
                versions.push(tag.to_string());
            }
        }
        if versions.is_empty() {
            return Err(Error::Other(format!(
                "{configuration}/{directory}@{reference} pins no {} tag",
                application.image
            )));
        }
        Ok(versions)
    }

    async fn application_status(
        &self,
        application: &DeployedApplication,
        commit: &str,
    ) -> Result<(bool, Vec<String>)> {
        let versions = self.pinned_versions(application).await?;
        let contained = try_join_all(versions.iter().map(|version| {
            self.client
                .commit_contained(&application.repository, commit, version)
        }))
        .await?;
        Ok((contained.into_iter().all(|inside| inside), versions))
    }

    async fn package_status(
        &self,
        application: &DeployedApplication,
        package: &DeployedPackage,
        commit: &str,
    ) -> Result<(bool, Vec<String>)> {
        let versions = self.pinned_versions(application).await?;
        let contained = try_join_all(
            versions
                .iter()
                .map(|version| self.package_contains(application, package, version, commit)),
        )
        .await?;
        Ok((contained.into_iter().all(|inside| inside), versions))
    }

    async fn package_contains(
        &self,
        application: &DeployedApplication,
        package: &DeployedPackage,
        version: &str,
        commit: &str,
    ) -> Result<bool> {
        let lock = self
            .client
            .get_file_at_ref(&application.repository, &package.lock_file, version)
            .await?
            .ok_or_else(|| {
                Error::Other(format!(
                    "{}/{}@{version} does not exist",
                    application.repository, package.lock_file
                ))
            })?;
        let locked = locked_package(&lock, &package.package).ok_or_else(|| {
            Error::Other(format!(
                "{}/{}@{version} does not lock {}",
                application.repository, package.lock_file, package.package
            ))
        })?;
        let reference =
            validated_reference(locked.reference.as_deref().unwrap_or(&locked.version))?;
        self.client
            .commit_contained(&package.repository, commit, reference)
            .await
    }
}

#[async_trait]
impl<C: HttpClient + 'static> DeploymentStatus for DeploymentResolver<C> {
    fn role(&self, repository: &str) -> RepositoryRole {
        let applications = &self.topology.applications;
        if applications
            .iter()
            .any(|application| same_repository(&application.repository, repository))
        {
            RepositoryRole::Application
        } else if applications
            .iter()
            .flat_map(|application| &application.packages)
            .any(|package| same_repository(&package.repository, repository))
        {
            RepositoryRole::Package
        } else {
            RepositoryRole::Untracked
        }
    }

    async fn pull_request(
        &self,
        repository: &str,
        number: i64,
    ) -> Result<Option<PullRequestState>> {
        self.client.get_pull_request_state(repository, number).await
    }

    async fn commit_status(&self, repository: &str, commit: &str) -> Result<LiveStatus> {
        if !is_commit_sha(commit) {
            return Err(Error::Other(format!("{commit:?} is not a commit SHA")));
        }
        let mut checks = Vec::new();
        for application in &self.topology.applications {
            if same_repository(&application.repository, repository) {
                checks.push(self.application_status(application, commit).await?);
            }
            for package in &application.packages {
                if same_repository(&package.repository, repository) {
                    checks.push(self.package_status(application, package, commit).await?);
                }
            }
        }
        if checks.is_empty() {
            return Ok(LiveStatus::Untracked);
        }

        let mut versions = Vec::new();
        let mut live = true;
        for (contained, application_versions) in checks {
            live &= contained;
            for version in application_versions {
                if !versions.contains(&version) {
                    versions.push(version);
                }
            }
        }
        Ok(if live {
            LiveStatus::Live { versions }
        } else {
            LiveStatus::Pending { versions }
        })
    }
}

/// Every `image:` mapping in a YAML values file, found by an indentation-aware
/// line scan that tracks the key path. Values are trimmed and unquoted; an
/// alias (`*name`) is unresolvable and left out.
pub fn image_references(yaml: &str) -> Vec<ImageReference> {
    ImageScanner::default().scan(yaml)
}

/// The locked version of `package` in a `composer.lock`, from `packages` or
/// `packages-dev`, with its source (else dist) reference.
pub fn locked_package(lock: &str, package: &str) -> Option<LockedPackage> {
    #[derive(Deserialize)]
    struct ComposerLock {
        #[serde(default)]
        packages: Vec<ComposerPackage>,
        #[serde(rename = "packages-dev", default)]
        packages_dev: Vec<ComposerPackage>,
    }

    #[derive(Deserialize)]
    struct ComposerPackage {
        name: String,
        version: String,
        source: Option<ComposerSource>,
        dist: Option<ComposerSource>,
    }

    #[derive(Deserialize)]
    struct ComposerSource {
        reference: Option<String>,
    }

    let lock: ComposerLock = serde_json::from_str(lock).ok()?;
    lock.packages
        .into_iter()
        .chain(lock.packages_dev)
        .find(|candidate| candidate.name == package)
        .map(|found| LockedPackage {
            version: found.version,
            reference: [found.source, found.dist]
                .into_iter()
                .flatten()
                .find_map(|source| source.reference.filter(|reference| !reference.is_empty())),
        })
}

fn is_commit_sha(value: &str) -> bool {
    (7..=40).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// `value` when it is safe to place in a GitHub API path as a git ref.
fn validated_reference(value: &str) -> Result<&str> {
    let safe = !value.is_empty()
        && !value.starts_with('-')
        && !value.contains("..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'));
    if safe {
        Ok(value)
    } else {
        Err(Error::Other(format!("{value:?} is not a usable git ref")))
    }
}

fn same_repository(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn is_values_file(path: &str) -> bool {
    let name = file_name(path);
    (name.ends_with(".yaml") || name.ends_with(".yml")) && name != CHART_FILE
}

struct ScopeEntry {
    indent: usize,
    segment: String,
    image: Option<usize>,
}

#[derive(Default)]
struct ImageScanner {
    scopes: Vec<ScopeEntry>,
    references: Vec<ImageReference>,
    block_scalar_indent: Option<usize>,
}

impl ImageScanner {
    fn scan(mut self, yaml: &str) -> Vec<ImageReference> {
        for raw in yaml.lines() {
            self.line(raw);
        }
        self.references
            .into_iter()
            .filter(|reference| reference.repository.is_some() || reference.tag.is_some())
            .collect()
    }

    fn line(&mut self, raw: &str) {
        let content = strip_comment(raw).trim_end();
        let trimmed = content.trim_start();
        if trimmed.is_empty() {
            return;
        }
        let indent = content.len() - trimmed.len();
        if let Some(block) = self.block_scalar_indent {
            if indent > block {
                return;
            }
            self.block_scalar_indent = None;
        }
        if trimmed == "---" || trimmed == "..." {
            self.scopes.clear();
            return;
        }

        let mut indent = indent;
        let mut entry = trimmed;
        if is_list_item(entry) {
            self.scopes.retain(|scope| {
                scope.indent < indent
                    || (scope.indent == indent && scope.segment != LIST_ITEM_SEGMENT)
            });
            self.scopes.push(ScopeEntry {
                indent,
                segment: LIST_ITEM_SEGMENT.to_string(),
                image: None,
            });
            let item = entry[1..].trim_start();
            indent += entry.len() - item.len();
            entry = item;
            if entry.is_empty() {
                return;
            }
        } else {
            self.scopes.retain(|scope| scope.indent < indent);
        }

        let Some((key, value)) = split_key(entry) else {
            return;
        };
        let value = strip_anchor(value);
        if is_block_scalar(value) {
            self.block_scalar_indent = Some(indent);
            return;
        }
        if value.is_empty() {
            self.open_mapping(indent, key);
        } else if let Some(flow) = value
            .strip_prefix('{')
            .and_then(|rest| rest.strip_suffix('}'))
        {
            self.open_mapping(indent, key);
            for (child, child_value) in flow.split(',').filter_map(split_key) {
                self.assign(indent + 1, child, child_value);
            }
            self.scopes.pop();
        } else {
            self.assign(indent, key, value);
        }
    }

    fn open_mapping(&mut self, indent: usize, key: &str) {
        let image = (key == IMAGE_KEY).then(|| {
            self.references.push(ImageReference {
                path: self.path_with(key),
                repository: None,
                tag: None,
            });
            self.references.len() - 1
        });
        self.scopes.push(ScopeEntry {
            indent,
            segment: key.to_string(),
            image,
        });
    }

    fn assign(&mut self, indent: usize, key: &str, value: &str) {
        let Some(index) = self
            .scopes
            .last()
            .filter(|scope| scope.indent < indent)
            .and_then(|scope| scope.image)
        else {
            return;
        };
        let value = unquote(value.trim());
        if value.is_empty() || value.starts_with('*') {
            return;
        }
        let reference = &mut self.references[index];
        match key {
            REPOSITORY_KEY => reference.repository = Some(value.to_string()),
            TAG_KEY => reference.tag = Some(value.to_string()),
            _ => {}
        }
    }

    fn path_with(&self, key: &str) -> String {
        let mut path = String::new();
        for segment in self
            .scopes
            .iter()
            .map(|scope| scope.segment.as_str())
            .chain([key])
        {
            if segment == LIST_ITEM_SEGMENT {
                path.push_str(LIST_ITEM_SEGMENT);
            } else {
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(segment);
            }
        }
        path
    }
}

fn is_list_item(entry: &str) -> bool {
    entry == "-" || entry.starts_with("- ")
}

fn is_block_scalar(value: &str) -> bool {
    let mut characters = value.chars();
    matches!(characters.next(), Some('|' | '>'))
        && characters.all(|character| matches!(character, '-' | '+' | '0'..='9'))
}

fn strip_anchor(value: &str) -> &str {
    match value.strip_prefix('&') {
        Some(anchored) => anchored
            .split_once(char::is_whitespace)
            .map_or("", |(_, rest)| rest.trim_start()),
        None => value,
    }
}

/// The key and value of a `key: value` or `key:` entry, split at the first
/// colon outside quotes that ends the line or is followed by whitespace.
fn split_key(entry: &str) -> Option<(&str, &str)> {
    let mut quote = None;
    let mut previous = None;
    for (index, character) in entry.char_indices() {
        match (quote, character) {
            (None, '"' | '\'') if opens_quote(previous) => quote = Some(character),
            (Some(open), _) if character == open => quote = None,
            (None, ':') => {
                let rest = &entry[index + 1..];
                if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                    let key = unquote(entry[..index].trim());
                    return (!key.is_empty()).then_some((key, rest.trim()));
                }
            }
            _ => {}
        }
        previous = Some(character);
    }
    None
}

/// `line` without a trailing comment: a `#` outside quotes that starts the
/// line or follows whitespace.
fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    let mut previous = None;
    for (index, character) in line.char_indices() {
        match (quote, character) {
            (None, '"' | '\'') if opens_quote(previous) => quote = Some(character),
            (Some(open), _) if character == open => quote = None,
            (None, '#') if previous.is_none_or(char::is_whitespace) => return &line[..index],
            _ => {}
        }
        previous = Some(character);
    }
    line
}

/// Whether a quote after `previous` starts a quoted scalar rather than sitting
/// inside a plain one, such as the apostrophe in `don't`.
fn opens_quote(previous: Option<char>) -> bool {
    previous.is_none_or(|character| {
        character.is_whitespace() || matches!(character, ':' | '[' | '{' | ',')
    })
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use abnegate_http::HttpResponse;
    use base64::Engine;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;

    const CLOUD_IMAGE: &str = "appwrite/cloud";
    const CLOUD_REPOSITORY: &str = "appwrite-labs/cloud";
    const CE_REPOSITORY: &str = "appwrite/appwrite";
    const CE_PACKAGE: &str = "appwrite/server-ce";
    const CONFIGURATION_REPOSITORY: &str = "appwrite-labs/application-configuration";
    const CLOUD_DIRECTORY: &str = "cloud/production/cloud";
    const BACKGROUND_DIRECTORY: &str = "cloud/production/cloud-background";
    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
    const CE_REFERENCE: &str = "a7e4093205ad79f674cc65de301e1ef6ce1cdb43";

    /// The `global` block and a sibling image of
    /// `cloud/production/cloud/default.yaml`, trimmed to what the scan reads.
    const CLOUD_DEFAULT: &str = r#"x-hardened-security-context: &hardened-security-context
  allowPrivilegeEscalation: false
  capabilities:
    drop: [NET_RAW]
global:
  # The default ndots:5 sends every lookup of a dotted external name through
  # the three search domains first.
  dnsConfig:
    options:
      - name: ndots
        value: "2"
  securityContext: *hardened-security-context
  image:
    repository: appwrite/cloud
    tag: 1.53.30
  imagePullSecrets:
    - name: dockerhub
cloud:
  initContainers:
    - name: php-fast-profile
      # Pin the tested multi-architecture image; this container can read API memory.
      image: ghcr.io/loks0n/php-fast-profile:0.3.0@sha256:04a7571427fb9af4bb48329c371aecd9fc37c5ed506925c938901b761f68536a
      restartPolicy: Always
cloud-geo-premium:
  fullnameOverride: appwrite-geo-premium
  selectorLabels:
    app: appwrite-geo-premium
  image:
    repository: appwrite/geo-premium
    tag: "0.2.3"
  replicaCount: null
"#;

    /// The `global` block of `cloud/production/cloud-background/default.yaml`.
    const BACKGROUND_DEFAULT: &str = r#"x-hardened-security-context: &hardened-security-context
  allowPrivilegeEscalation: false
global:
  dnsConfig:
    options:
      - name: ndots
        value: "2"
  securityContext: *hardened-security-context
  image:
    repository: appwrite/cloud
    tag: 1.53.30
  imagePullSecrets:
    - name: dockerhub
worker-audits:
  replicaCount: 2
"#;

    /// The `appwrite/server-ce` entry of appwrite-labs/cloud's `composer.lock`
    /// at 1.53.30.
    fn cloud_lock(reference: &str) -> String {
        format!(
            r#"{{
    "_readme": ["This file locks the dependencies of your project to a known state"],
    "content-hash": "0",
    "packages": [
        {{
            "name": "appwrite/server-ce",
            "version": "dev-main",
            "source": {{
                "type": "git",
                "url": "https://github.com/appwrite/appwrite.git",
                "reference": "{reference}"
            }},
            "dist": {{
                "type": "zip",
                "url": "https://api.github.com/repos/appwrite/appwrite/zipball/{reference}",
                "reference": "{reference}",
                "shasum": ""
            }}
        }}
    ],
    "packages-dev": []
}}"#
        )
    }

    #[derive(Default)]
    struct RoutedHttpClient {
        routes: HashMap<String, HttpResponse>,
        requested: Arc<Mutex<Vec<String>>>,
    }

    impl RoutedHttpClient {
        fn route(mut self, url: String, status: u16, body: impl Into<String>) -> Self {
            self.routes
                .insert(url, HttpResponse::new(status, body.into()));
            self
        }

        fn directory(self, repository: &str, directory: &str, files: &[&str]) -> Self {
            let entries: Vec<serde_json::Value> = files
                .iter()
                .map(|file| {
                    serde_json::json!({"name": file, "path": format!("{directory}/{file}"), "type": "file"})
                })
                .collect();
            self.route(
                contents_url(repository, directory, "main"),
                200,
                serde_json::Value::Array(entries).to_string(),
            )
        }

        fn file(self, repository: &str, path: &str, git_ref: &str, content: &str) -> Self {
            let encoded = base64::engine::general_purpose::STANDARD.encode(content);
            self.route(
                contents_url(repository, path, git_ref),
                200,
                serde_json::json!({"content": encoded, "encoding": "base64"}).to_string(),
            )
        }

        fn compare(self, repository: &str, commit: &str, git_ref: &str, status: &str) -> Self {
            self.route(
                compare_url(repository, commit, git_ref),
                200,
                serde_json::json!({"status": status}).to_string(),
            )
        }
    }

    #[async_trait]
    impl HttpClient for RoutedHttpClient {
        async fn get(
            &self,
            url: &str,
            _headers: Vec<(&str, String)>,
        ) -> abnegate_http::Result<HttpResponse> {
            self.requested.lock().unwrap().push(url.to_string());
            Ok(self.routes.get(url).cloned().unwrap_or_else(|| {
                HttpResponse::new(404, r#"{"message":"Not Found"}"#.to_string())
            }))
        }
    }

    fn contents_url(repository: &str, path: &str, git_ref: &str) -> String {
        format!("https://api.github.com/repos/{repository}/contents/{path}?ref={git_ref}")
    }

    fn compare_url(repository: &str, commit: &str, git_ref: &str) -> String {
        format!("https://api.github.com/repos/{repository}/compare/{commit}...{git_ref}?per_page=1")
    }

    fn cloud_application() -> DeployedApplication {
        DeployedApplication {
            repository: CLOUD_REPOSITORY.to_string(),
            image: CLOUD_IMAGE.to_string(),
            directories: vec![
                CLOUD_DIRECTORY.to_string(),
                BACKGROUND_DIRECTORY.to_string(),
            ],
            packages: vec![DeployedPackage {
                repository: CE_REPOSITORY.to_string(),
                package: CE_PACKAGE.to_string(),
                lock_file: "composer.lock".to_string(),
            }],
        }
    }

    fn topology() -> DeploymentTopology {
        DeploymentTopology {
            configuration_repository: CONFIGURATION_REPOSITORY.to_string(),
            configuration_reference: "main".to_string(),
            applications: vec![cloud_application()],
        }
    }

    fn region_override(tag: &str) -> String {
        format!("global:\n  image:\n    tag: {tag}\ncloud-geo-premium:\n  image:\n    tag: 0.2.4\n")
    }

    /// The production configuration: cloud pins 1.53.30 by default and fra1
    /// overrides it to 1.53.31; cloud-background pins 1.53.30 everywhere.
    fn production() -> RoutedHttpClient {
        RoutedHttpClient::default()
            .directory(
                CONFIGURATION_REPOSITORY,
                CLOUD_DIRECTORY,
                &["Chart.yaml", "README.md", "default.yaml", "fra1.yaml"],
            )
            .file(
                CONFIGURATION_REPOSITORY,
                &format!("{CLOUD_DIRECTORY}/default.yaml"),
                "main",
                CLOUD_DEFAULT,
            )
            .file(
                CONFIGURATION_REPOSITORY,
                &format!("{CLOUD_DIRECTORY}/fra1.yaml"),
                "main",
                &region_override("1.53.31"),
            )
            .directory(
                CONFIGURATION_REPOSITORY,
                BACKGROUND_DIRECTORY,
                &["Chart.yaml", "default.yaml", "nyc3-nats.yaml"],
            )
            .file(
                CONFIGURATION_REPOSITORY,
                &format!("{BACKGROUND_DIRECTORY}/default.yaml"),
                "main",
                BACKGROUND_DEFAULT,
            )
            .file(
                CONFIGURATION_REPOSITORY,
                &format!("{BACKGROUND_DIRECTORY}/nyc3-nats.yaml"),
                "main",
                "worker-audits:\n  replicaCount: 3\n",
            )
    }

    fn resolver(client: RoutedHttpClient) -> DeploymentResolver<RoutedHttpClient> {
        DeploymentResolver::with_client(
            ReleaseClient::with_http_client("token", client),
            topology(),
        )
    }

    fn versions(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn test_image_references_reads_the_cloud_default_values() {
        assert_eq!(
            image_references(CLOUD_DEFAULT),
            vec![
                ImageReference {
                    path: "global.image".to_string(),
                    repository: Some(CLOUD_IMAGE.to_string()),
                    tag: Some("1.53.30".to_string()),
                },
                ImageReference {
                    path: "cloud-geo-premium.image".to_string(),
                    repository: Some("appwrite/geo-premium".to_string()),
                    tag: Some("0.2.3".to_string()),
                },
            ]
        );
    }

    #[test]
    fn test_image_references_reads_the_cloud_background_default_values() {
        assert_eq!(
            image_references(BACKGROUND_DEFAULT),
            vec![ImageReference {
                path: "global.image".to_string(),
                repository: Some(CLOUD_IMAGE.to_string()),
                tag: Some("1.53.30".to_string()),
            }]
        );
    }

    #[test]
    fn test_image_references_reads_a_region_override_without_a_repository() {
        assert_eq!(
            image_references(&region_override("1.53.31")),
            vec![
                ImageReference {
                    path: "global.image".to_string(),
                    repository: None,
                    tag: Some("1.53.31".to_string()),
                },
                ImageReference {
                    path: "cloud-geo-premium.image".to_string(),
                    repository: None,
                    tag: Some("0.2.4".to_string()),
                },
            ]
        );
    }

    #[test]
    fn test_image_references_tracks_list_items_in_the_path() {
        let yaml = "cloud:\n  sidecars:\n  - name: agent\n    image:\n      repository: 'appwrite/agent' # pinned\n      tag: \"1.0\"\n  - name: other\n    image: {repository: appwrite/other, tag: 2.0}\n";
        assert_eq!(
            image_references(yaml),
            vec![
                ImageReference {
                    path: "cloud.sidecars[].image".to_string(),
                    repository: Some("appwrite/agent".to_string()),
                    tag: Some("1.0".to_string()),
                },
                ImageReference {
                    path: "cloud.sidecars[].image".to_string(),
                    repository: Some("appwrite/other".to_string()),
                    tag: Some("2.0".to_string()),
                },
            ]
        );
    }

    #[test]
    fn test_image_references_ignores_block_scalars_aliases_and_deeper_keys() {
        let yaml = "global:\n  notes: |\n    image:\n      tag: 9.9.9\n  image: &cloud-image\n    repository: appwrite/cloud\n    tag: *pinned\n    extra:\n      tag: 8.8.8\n";
        assert_eq!(
            image_references(yaml),
            vec![ImageReference {
                path: "global.image".to_string(),
                repository: Some(CLOUD_IMAGE.to_string()),
                tag: None,
            }]
        );
    }

    #[test]
    fn test_image_references_strips_comments_after_plain_apostrophes() {
        let yaml = "global:\n  image:\n    repository: appwrite/cloud\n    tag: 1.0\n  note: don't # it's 'quoted'\n";
        assert_eq!(
            image_references(yaml),
            vec![ImageReference {
                path: "global.image".to_string(),
                repository: Some(CLOUD_IMAGE.to_string()),
                tag: Some("1.0".to_string()),
            }]
        );
        assert_eq!(strip_comment("  note: don't # it's"), "  note: don't ");
    }

    #[test]
    fn test_image_references_keeps_hashes_inside_quotes() {
        let yaml =
            "global:\n  image:\n    repository: \"appwrite/cloud#edge\"\n    tag: 1.0 # comment\n";
        assert_eq!(
            image_references(yaml),
            vec![ImageReference {
                path: "global.image".to_string(),
                repository: Some("appwrite/cloud#edge".to_string()),
                tag: Some("1.0".to_string()),
            }]
        );
    }

    #[test]
    fn test_locked_package_reads_the_server_ce_dev_main_reference() {
        assert_eq!(
            locked_package(&cloud_lock(CE_REFERENCE), CE_PACKAGE),
            Some(LockedPackage {
                version: "dev-main".to_string(),
                reference: Some(CE_REFERENCE.to_string()),
            })
        );
    }

    #[test]
    fn test_locked_package_reads_packages_dev_and_misses_unknown_packages() {
        let lock = r#"{"packages": [], "packages-dev": [{"name": "phpunit/phpunit", "version": "12.0.0"}]}"#;
        assert_eq!(
            locked_package(lock, "phpunit/phpunit"),
            Some(LockedPackage {
                version: "12.0.0".to_string(),
                reference: None,
            })
        );
        assert_eq!(locked_package(lock, CE_PACKAGE), None);
        assert_eq!(locked_package("not json", CE_PACKAGE), None);
    }

    #[tokio::test]
    async fn test_pinned_versions_counts_region_overrides_and_ignores_other_images() {
        let versions_pinned = resolver(production())
            .pinned_versions(&cloud_application())
            .await
            .unwrap();

        assert_eq!(versions_pinned, versions(&["1.53.30", "1.53.31"]));
    }

    #[tokio::test]
    async fn test_pinned_versions_without_a_default_file_is_an_error() {
        let client = RoutedHttpClient::default()
            .directory(
                CONFIGURATION_REPOSITORY,
                CLOUD_DIRECTORY,
                &["Chart.yaml", "fra1.yaml"],
            )
            .file(
                CONFIGURATION_REPOSITORY,
                &format!("{CLOUD_DIRECTORY}/fra1.yaml"),
                "main",
                &region_override("1.53.31"),
            );

        let error = resolver(client)
            .pinned_versions(&cloud_application())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("default.yaml"), "{error}");
    }

    #[tokio::test]
    async fn test_pinned_versions_without_any_tag_is_an_error() {
        let client = RoutedHttpClient::default()
            .directory(CONFIGURATION_REPOSITORY, CLOUD_DIRECTORY, &["default.yaml"])
            .file(
                CONFIGURATION_REPOSITORY,
                &format!("{CLOUD_DIRECTORY}/default.yaml"),
                "main",
                "global:\n  image:\n    repository: appwrite/other\n    tag: 1.0.0\n",
            );

        let error = resolver(client)
            .pinned_versions(&cloud_application())
            .await
            .unwrap_err();

        assert!(
            error.to_string().contains("pins no appwrite/cloud tag"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn test_pinned_versions_with_a_missing_directory_is_an_error() {
        assert!(resolver(RoutedHttpClient::default())
            .pinned_versions(&cloud_application())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_commit_status_is_live_only_when_every_pinned_tag_contains_the_commit() {
        let client = production()
            .compare(CLOUD_REPOSITORY, COMMIT, "1.53.30", "ahead")
            .compare(CLOUD_REPOSITORY, COMMIT, "1.53.31", "identical");

        let status = resolver(client)
            .commit_status(CLOUD_REPOSITORY, COMMIT)
            .await
            .unwrap();

        assert_eq!(
            status,
            LiveStatus::Live {
                versions: versions(&["1.53.30", "1.53.31"])
            }
        );
    }

    #[tokio::test]
    async fn test_commit_status_is_pending_while_one_pinned_tag_lacks_the_commit() {
        let client = production()
            .compare(CLOUD_REPOSITORY, COMMIT, "1.53.30", "diverged")
            .compare(CLOUD_REPOSITORY, COMMIT, "1.53.31", "ahead");

        let status = resolver(client)
            .commit_status(CLOUD_REPOSITORY, COMMIT)
            .await
            .unwrap();

        assert_eq!(
            status,
            LiveStatus::Pending {
                versions: versions(&["1.53.30", "1.53.31"])
            }
        );
    }

    #[tokio::test]
    async fn test_commit_status_of_a_package_goes_through_the_locked_reference() {
        let client = production()
            .file(
                CLOUD_REPOSITORY,
                "composer.lock",
                "1.53.30",
                &cloud_lock(CE_REFERENCE),
            )
            .file(
                CLOUD_REPOSITORY,
                "composer.lock",
                "1.53.31",
                &cloud_lock(CE_REFERENCE),
            )
            .compare(CE_REPOSITORY, COMMIT, CE_REFERENCE, "ahead");

        let status = resolver(client)
            .commit_status(CE_REPOSITORY, COMMIT)
            .await
            .unwrap();

        assert_eq!(
            status,
            LiveStatus::Live {
                versions: versions(&["1.53.30", "1.53.31"])
            }
        );
    }

    #[tokio::test]
    async fn test_commit_status_of_a_package_without_its_lock_file_is_an_error() {
        let client = production()
            .file(
                CLOUD_REPOSITORY,
                "composer.lock",
                "1.53.30",
                &cloud_lock(CE_REFERENCE),
            )
            .compare(CE_REPOSITORY, COMMIT, CE_REFERENCE, "ahead");

        assert!(resolver(client)
            .commit_status(CE_REPOSITORY, COMMIT)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_commit_status_of_an_unmapped_repository_is_untracked() {
        let routed = RoutedHttpClient::default();
        let status = resolver(routed)
            .commit_status("appwrite/console", COMMIT)
            .await
            .unwrap();

        assert_eq!(status, LiveStatus::Untracked);
    }

    #[tokio::test]
    async fn test_commit_status_with_a_missing_comparison_is_an_error() {
        let client = production().compare(CLOUD_REPOSITORY, COMMIT, "1.53.30", "ahead");

        assert!(resolver(client)
            .commit_status(CLOUD_REPOSITORY, COMMIT)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_commit_status_rejects_an_invalid_sha_before_any_request() {
        for commit in ["", "abc", "ABCDEF0", "0123456../main", &"a".repeat(41)] {
            let client = production();
            let requested = Arc::clone(&client.requested);
            assert!(
                resolver(client)
                    .commit_status(CLOUD_REPOSITORY, commit)
                    .await
                    .is_err(),
                "{commit:?} must be rejected"
            );
            assert!(
                requested.lock().unwrap().is_empty(),
                "{commit:?} was requested"
            );
        }
    }

    #[tokio::test]
    async fn test_commit_status_rejects_an_unsafe_pinned_tag() {
        let client = RoutedHttpClient::default()
            .directory(CONFIGURATION_REPOSITORY, CLOUD_DIRECTORY, &["default.yaml"])
            .file(
                CONFIGURATION_REPOSITORY,
                &format!("{CLOUD_DIRECTORY}/default.yaml"),
                "main",
                "global:\n  image:\n    repository: appwrite/cloud\n    tag: main?x=1\n",
            );

        assert!(resolver(client)
            .commit_status(CLOUD_REPOSITORY, COMMIT)
            .await
            .is_err());
    }

    #[test]
    fn test_role_maps_applications_and_packages() {
        let resolver = resolver(RoutedHttpClient::default());

        assert_eq!(resolver.role(CLOUD_REPOSITORY), RepositoryRole::Application);
        assert_eq!(
            resolver.role("Appwrite-Labs/Cloud"),
            RepositoryRole::Application
        );
        assert_eq!(resolver.role(CE_REPOSITORY), RepositoryRole::Package);
        assert_eq!(resolver.role("appwrite/console"), RepositoryRole::Untracked);
    }

    #[tokio::test]
    async fn test_pull_request_reports_closed_and_unmerged() {
        let client = RoutedHttpClient::default().route(
            format!("https://api.github.com/repos/{CE_REPOSITORY}/pulls/7"),
            200,
            r#"{"state": "closed", "merged": false, "merge_commit_sha": "0123456789abcdef0123456789abcdef01234567", "merged_at": null}"#,
        );

        let state = resolver(client)
            .pull_request(CE_REPOSITORY, 7)
            .await
            .unwrap();

        assert_eq!(
            state,
            Some(PullRequestState {
                merged: false,
                closed: true,
                merge_commit_sha: None,
                merged_at: None,
            })
        );
    }
}

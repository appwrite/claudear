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
use std::collections::HashMap;

const DEFAULT_VALUES_FILE: &str = "default.yaml";
const CHART_FILE: &str = "Chart.yaml";
const IMAGE_KEY: &str = "image";
const REPOSITORY_KEY: &str = "repository";
const TAG_KEY: &str = "tag";
const LIST_ITEM_SEGMENT: &str = "[]";
const MERGE_KEY: &str = "<<";
const DOCKER_HUB_PREFIXES: [&str; 3] = ["docker.io/", "index.docker.io/", "registry-1.docker.io/"];

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

/// How an `image:` entry is written: a mapping with `repository` and `tag`
/// keys, or a single `repository:tag` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageForm {
    Mapping,
    Scalar,
}

/// An `image:` entry in a values file. `path` is its key path, such as
/// `global.image` or `cloud.initContainers[].image`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageReference {
    pub path: String,
    pub form: ImageForm,
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

        let references: Vec<Vec<ImageReference>> = files
            .iter()
            .zip(&contents)
            .map(|(path, yaml)| {
                image_references(yaml).map_err(|error| {
                    Error::Other(format!("{configuration}/{path}@{reference}: {error}"))
                })
            })
            .collect::<Result<_>>()?;
        let defaults = DefaultImages::new(&references[default_file], &application.image);

        let mut versions = Vec::new();
        for (index, file_references) in references.iter().enumerate() {
            let is_default = index == default_file;
            for image in file_references {
                let tag = defaults.pinned_tag(image, is_default).map_err(|error| {
                    Error::Other(format!(
                        "{configuration}/{}@{reference}: {error}",
                        files[index]
                    ))
                })?;
                let Some(tag) = tag else {
                    continue;
                };
                let tag = validated_reference(tag)?;
                if !versions.iter().any(|version| version == tag) {
                    versions.push(tag.to_string());
                }
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

/// Every `image:` entry in a YAML values file, found by an indentation-aware
/// line scan that tracks the key path. Anything the scan cannot read with
/// certainty is an error rather than a skipped entry: an alias, anchor or merge
/// key in an image block, a block or flow value it cannot parse, a nested
/// `repository`/`tag`, or a tag YAML would load as a number, boolean or null.
pub fn image_references(yaml: &str) -> Result<Vec<ImageReference>> {
    ImageScanner::default().scan(yaml).map_err(Error::Other)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ownership {
    Application,
    Other,
    Mixed,
}

/// Which image each `default.yaml` image path names, so that an override that
/// sets only a tag can be attributed to the image it changes.
struct DefaultImages<'a> {
    image: &'a str,
    owners: HashMap<&'a str, Ownership>,
}

impl<'a> DefaultImages<'a> {
    fn new(references: &'a [ImageReference], image: &'a str) -> Self {
        let mut owners: HashMap<&str, Ownership> = HashMap::new();
        for reference in references {
            let Some(repository) = reference.repository.as_deref() else {
                continue;
            };
            let owner = if names_image(repository, image) {
                Ownership::Application
            } else {
                Ownership::Other
            };
            owners
                .entry(reference.path.as_str())
                .and_modify(|existing| {
                    if *existing != owner {
                        *existing = Ownership::Mixed;
                    }
                })
                .or_insert(owner);
        }
        Self { image, owners }
    }

    /// The tag `reference` pins for the application image, `None` when it pins
    /// another image or inherits the default tag, and an error when it cannot
    /// be attributed with certainty.
    fn pinned_tag<'r>(
        &self,
        reference: &'r ImageReference,
        is_default: bool,
    ) -> std::result::Result<Option<&'r str>, String> {
        let image = self.image;
        let path = reference.path.as_str();
        let owner = self.owners.get(path).copied();
        let overrides_default_path = !is_default && !path.contains(LIST_ITEM_SEGMENT);
        let tag = reference.tag.as_deref();
        match reference.repository.as_deref() {
            Some(repository) if names_image(repository, image) => match (reference.form, tag) {
                (ImageForm::Scalar, _) => Err(format!(
                    "{path} pins {image} as a single image string, which is not tracked"
                )),
                (ImageForm::Mapping, Some(tag)) => Ok(Some(tag)),
                (ImageForm::Mapping, None)
                    if overrides_default_path && owner == Some(Ownership::Application) =>
                {
                    Ok(None)
                }
                (ImageForm::Mapping, None) => Err(format!("{path} names {image} without a tag")),
            },
            Some(_)
                if overrides_default_path
                    && matches!(owner, Some(Ownership::Application | Ownership::Mixed)) =>
            {
                Err(format!("{path} replaces {image} with another image"))
            }
            Some(_) => Ok(None),
            None => match tag {
                None => Ok(None),
                Some(_) if !overrides_default_path => {
                    Err(format!("{path} sets a tag without a repository"))
                }
                Some(tag) => match owner {
                    Some(Ownership::Application) => Ok(Some(tag)),
                    Some(Ownership::Other) => Ok(None),
                    Some(Ownership::Mixed) | None => Err(format!(
                        "{path} sets a tag that cannot be attributed to an image in {DEFAULT_VALUES_FILE}"
                    )),
                },
            },
        }
    }
}

fn names_image(repository: &str, image: &str) -> bool {
    fn normalized(name: &str) -> String {
        let lower = name.trim().to_ascii_lowercase();
        DOCKER_HUB_PREFIXES
            .iter()
            .find_map(|prefix| lower.strip_prefix(prefix))
            .map_or_else(|| lower.clone(), str::to_string)
    }
    normalized(repository) == normalized(image)
}

/// The repository and tag of a `repository[:tag][@digest]` image string.
fn split_image(value: &str) -> (String, Option<String>) {
    let undigested = value.split_once('@').map_or(value, |(name, _)| name);
    let name_start = undigested.rfind('/').map_or(0, |slash| slash + 1);
    match undigested[name_start..].rfind(':') {
        Some(colon) => {
            let split = name_start + colon;
            (
                undigested[..split].to_string(),
                Some(undigested[split + 1..].to_string()),
            )
        }
        None => (undigested.to_string(), None),
    }
}

struct ScopeEntry {
    indent: usize,
    segment: String,
    image: Option<usize>,
    within_image: bool,
}

struct PendingFlow {
    text: String,
    depth: i64,
}

#[derive(Default)]
struct ImageScanner {
    scopes: Vec<ScopeEntry>,
    references: Vec<ImageReference>,
    block_scalar_indent: Option<usize>,
    flow: Option<PendingFlow>,
    line_number: usize,
}

type ScanResult<T> = std::result::Result<T, String>;

impl ImageScanner {
    fn scan(mut self, yaml: &str) -> ScanResult<Vec<ImageReference>> {
        for raw in yaml.lines() {
            self.line_number += 1;
            self.line(raw)
                .map_err(|error| format!("line {}: {error}", self.line_number))?;
        }
        if self.flow.is_some() {
            return Err("unterminated flow collection".to_string());
        }
        Ok(self
            .references
            .into_iter()
            .filter(|reference| reference.repository.is_some() || reference.tag.is_some())
            .collect())
    }

    fn line(&mut self, raw: &str) -> ScanResult<()> {
        let content = strip_comment(raw).trim_end();
        let trimmed = content.trim_start();
        if let Some(flow) = self.flow.as_mut() {
            flow.text.push(' ');
            flow.text.push_str(trimmed);
            flow.depth += flow_balance(trimmed);
            if flow.depth <= 0 {
                let text = std::mem::take(&mut flow.text);
                self.flow = None;
                if mentions_image_key(&text) {
                    return Err("an image inside a multi-line flow collection".to_string());
                }
            }
            return Ok(());
        }
        if trimmed.is_empty() {
            return Ok(());
        }
        let indent = content.len() - trimmed.len();
        if let Some(block) = self.block_scalar_indent {
            if indent > block {
                return Ok(());
            }
            self.block_scalar_indent = None;
        }
        if trimmed == "---" || trimmed == "..." {
            self.scopes.clear();
            return Ok(());
        }

        let mut indent = indent;
        let mut entry = trimmed;
        if is_list_item(entry) {
            self.scopes.retain(|scope| {
                scope.indent < indent
                    || (scope.indent == indent && scope.segment != LIST_ITEM_SEGMENT)
            });
            let parent = self.scopes.last();
            if parent.is_some_and(|scope| scope.image.is_some()) {
                return Err(format!("{} is a sequence", self.path()));
            }
            let within_image = parent.is_some_and(|scope| scope.within_image);
            self.scopes.push(ScopeEntry {
                indent,
                segment: LIST_ITEM_SEGMENT.to_string(),
                image: None,
                within_image,
            });
            let item = entry[1..].trim_start();
            indent += entry.len() - item.len();
            entry = item;
            if entry.is_empty() {
                return Ok(());
            }
        } else {
            self.scopes.retain(|scope| scope.indent < indent);
        }

        let within_image = self.scopes.last().is_some_and(|scope| scope.within_image);
        let Some((key, value)) = split_key(entry) else {
            if within_image {
                return Err(format!("{} has an entry that is not a key", self.path()));
            }
            return Ok(());
        };
        if within_image {
            self.image_entry(indent, key, value)
        } else if key == IMAGE_KEY {
            self.image(indent, value)
        } else {
            self.entry(indent, key, value)
        }
    }

    fn entry(&mut self, indent: usize, key: &str, value: &str) -> ScanResult<()> {
        let value = strip_anchor(value);
        if is_block_scalar(value) {
            self.block_scalar_indent = Some(indent);
        } else if value.is_empty() {
            self.open(indent, key, None);
        } else if value.starts_with(['{', '[']) {
            let depth = flow_balance(value);
            if depth > 0 {
                self.flow = Some(PendingFlow {
                    text: value.to_string(),
                    depth,
                });
            } else if mentions_image_key(value) {
                return Err(format!(
                    "{} has an image inside a flow collection",
                    self.path_with(key)
                ));
            }
        }
        Ok(())
    }

    fn image(&mut self, indent: usize, value: &str) -> ScanResult<()> {
        let path = self.path_with(IMAGE_KEY);
        if value.starts_with(['&', '*']) {
            return Err(format!("{path} is anchored or aliased"));
        }
        if is_block_scalar(value) || value.starts_with('[') {
            return Err(format!("{path} is not a mapping or an image string"));
        }
        if value.is_empty() {
            let index = self.record(path, ImageForm::Mapping, None, None);
            self.open(indent, IMAGE_KEY, Some(index));
            return Ok(());
        }
        if let Some(flow) = value.strip_prefix('{') {
            let inner = flow
                .strip_suffix('}')
                .filter(|inner| !inner.contains(['{', '[', '}', ']']))
                .ok_or_else(|| format!("{path} is a flow mapping that cannot be read"))?;
            let index = self.record(path.clone(), ImageForm::Mapping, None, None);
            for piece in inner
                .split(',')
                .map(str::trim)
                .filter(|piece| !piece.is_empty())
            {
                let (child, child_value) = split_key(piece)
                    .ok_or_else(|| format!("{path} has an entry that cannot be read"))?;
                self.assign(index, &path, child, child_value)?;
            }
            return Ok(());
        }
        let name = scalar(value).map_err(|error| format!("{path} {error}"))?;
        if name.is_empty() {
            return Err(format!("{path} is empty"));
        }
        let (repository, tag) = split_image(name);
        self.record(path, ImageForm::Scalar, Some(repository), tag);
        Ok(())
    }

    fn image_entry(&mut self, indent: usize, key: &str, value: &str) -> ScanResult<()> {
        let path = self.path_with(key);
        if key == MERGE_KEY || value.starts_with(['&', '*']) {
            return Err(format!(
                "{path} uses an alias, anchor or merge key in an image block"
            ));
        }
        let direct = self.scopes.last().and_then(|scope| scope.image);
        let tracked = matches!(key, REPOSITORY_KEY | TAG_KEY);
        match direct {
            Some(index) if tracked => {
                let image_path = self.path();
                self.assign(index, &image_path, key, value)
            }
            _ if tracked || key == IMAGE_KEY => {
                Err(format!("{path} is nested too deeply in an image block"))
            }
            _ if is_block_scalar(value) => {
                self.block_scalar_indent = Some(indent);
                Ok(())
            }
            _ if value.is_empty() => {
                self.open(indent, key, None);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn assign(&mut self, index: usize, path: &str, key: &str, value: &str) -> ScanResult<()> {
        if key == MERGE_KEY || value.starts_with(['&', '*']) {
            return Err(format!(
                "{path} uses an alias, anchor or merge key in an image block"
            ));
        }
        if key != REPOSITORY_KEY && key != TAG_KEY {
            return Ok(());
        }
        if value.is_empty() || is_block_scalar(value) || value.starts_with(['{', '[']) {
            return Err(format!("{path}.{key} is not a single value"));
        }
        let value = scalar(value)
            .map_err(|error| format!("{path}.{key} {error}"))?
            .to_string();
        if value.is_empty() {
            return Err(format!("{path}.{key} is empty"));
        }
        let reference = &mut self.references[index];
        if key == REPOSITORY_KEY {
            reference.repository = Some(value);
        } else {
            reference.tag = Some(value);
        }
        Ok(())
    }

    fn record(
        &mut self,
        path: String,
        form: ImageForm,
        repository: Option<String>,
        tag: Option<String>,
    ) -> usize {
        self.references.push(ImageReference {
            path,
            form,
            repository,
            tag,
        });
        self.references.len() - 1
    }

    fn open(&mut self, indent: usize, key: &str, image: Option<usize>) {
        let within_image =
            image.is_some() || self.scopes.last().is_some_and(|scope| scope.within_image);
        self.scopes.push(ScopeEntry {
            indent,
            segment: key.to_string(),
            image,
            within_image,
        });
    }

    fn path(&self) -> String {
        self.joined(self.scopes.iter().map(|scope| scope.segment.as_str()))
    }

    fn path_with(&self, key: &str) -> String {
        self.joined(
            self.scopes
                .iter()
                .map(|scope| scope.segment.as_str())
                .chain([key]),
        )
    }

    fn joined<'s>(&self, segments: impl Iterator<Item = &'s str>) -> String {
        let mut path = String::new();
        for segment in segments {
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

/// The string a scalar value loads as, or an error when YAML would load it as
/// something other than a string.
fn scalar(value: &str) -> ScanResult<&str> {
    let unquoted = unquote(value);
    if unquoted.len() != value.len() {
        return Ok(unquoted);
    }
    if value.starts_with(['"', '\'']) {
        return Err("has an unterminated quote".to_string());
    }
    if loads_as_non_string(value) {
        return Err(format!(
            "{value:?} loads as a number, boolean or null; quote it"
        ));
    }
    Ok(value)
}

fn loads_as_non_string(value: &str) -> bool {
    const KEYWORDS: [&str; 11] = [
        "~", "null", "true", "false", "yes", "no", "on", "off", "y", "n", "nan",
    ];
    let lower = value.to_ascii_lowercase();
    KEYWORDS.contains(&lower.trim_start_matches('.'))
        || lower.trim_start_matches(['+', '-', '.']) == "inf"
        || value.parse::<f64>().is_ok()
        || lower
            .strip_prefix("0x")
            .or_else(|| lower.strip_prefix("0o"))
            .is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
}

/// Opened minus closed flow brackets outside quotes.
fn flow_balance(text: &str) -> i64 {
    let mut quote = None;
    let mut depth = 0;
    for character in text.chars() {
        match (quote, character) {
            (None, '"' | '\'') => quote = Some(character),
            (Some(open), _) if character == open => quote = None,
            (None, '{' | '[') => depth += 1,
            (None, '}' | ']') => depth -= 1,
            _ => {}
        }
    }
    depth
}

/// Whether flow-collection text contains an `image` key.
fn mentions_image_key(text: &str) -> bool {
    text.match_indices(IMAGE_KEY).any(|(index, _)| {
        let before = text[..index].trim_end_matches(['"', '\'']);
        let after = text[index + IMAGE_KEY.len()..].trim_start_matches(['"', '\'']);
        before
            .chars()
            .last()
            .is_none_or(|character| character.is_whitespace() || matches!(character, '{' | ','))
            && after.trim_start().starts_with(':')
    })
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
        production_with(&region_override("1.53.31"))
    }

    /// The production configuration with `fra1.yaml` of cloud replaced.
    fn production_with(fra1: &str) -> RoutedHttpClient {
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
                fra1,
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

    fn mapping(path: &str, repository: Option<&str>, tag: Option<&str>) -> ImageReference {
        ImageReference {
            path: path.to_string(),
            form: ImageForm::Mapping,
            repository: repository.map(str::to_string),
            tag: tag.map(str::to_string),
        }
    }

    fn scan_error(yaml: &str) -> String {
        image_references(yaml)
            .expect_err("the scan must fail closed")
            .to_string()
    }

    #[test]
    fn test_image_references_reads_the_cloud_default_values() {
        assert_eq!(
            image_references(CLOUD_DEFAULT).unwrap(),
            vec![
                mapping("global.image", Some(CLOUD_IMAGE), Some("1.53.30")),
                ImageReference {
                    path: "cloud.initContainers[].image".to_string(),
                    form: ImageForm::Scalar,
                    repository: Some("ghcr.io/loks0n/php-fast-profile".to_string()),
                    tag: Some("0.3.0".to_string()),
                },
                mapping(
                    "cloud-geo-premium.image",
                    Some("appwrite/geo-premium"),
                    Some("0.2.3")
                ),
            ]
        );
    }

    #[test]
    fn test_image_references_reads_the_cloud_background_default_values() {
        assert_eq!(
            image_references(BACKGROUND_DEFAULT).unwrap(),
            vec![mapping("global.image", Some(CLOUD_IMAGE), Some("1.53.30"))]
        );
    }

    #[test]
    fn test_image_references_reads_a_region_override_without_a_repository() {
        assert_eq!(
            image_references(&region_override("1.53.31")).unwrap(),
            vec![
                mapping("global.image", None, Some("1.53.31")),
                mapping("cloud-geo-premium.image", None, Some("0.2.4")),
            ]
        );
    }

    #[test]
    fn test_image_references_tracks_list_items_in_the_path() {
        let yaml = "cloud:\n  sidecars:\n  - name: agent\n    image:\n      repository: 'appwrite/agent' # pinned\n      tag: \"1.0\"\n  - name: other\n    image: {repository: appwrite/other, tag: 2.0.0}\n";
        assert_eq!(
            image_references(yaml).unwrap(),
            vec![
                mapping(
                    "cloud.sidecars[].image",
                    Some("appwrite/agent"),
                    Some("1.0")
                ),
                mapping(
                    "cloud.sidecars[].image",
                    Some("appwrite/other"),
                    Some("2.0.0")
                ),
            ]
        );
    }

    #[test]
    fn test_image_references_ignores_block_scalars_and_anchors_outside_images() {
        let yaml = "x-base: &base\n  tag: 7.7.7\nglobal:\n  <<: *base\n  notes: |\n    image:\n      tag: 9.9.9\n  securityContext: *hardened\n  image:\n    repository: appwrite/cloud\n    tag: 1.53.30\n";
        assert_eq!(
            image_references(yaml).unwrap(),
            vec![mapping("global.image", Some(CLOUD_IMAGE), Some("1.53.30"))]
        );
    }

    #[test]
    fn test_image_references_rejects_aliases_anchors_and_merge_keys_in_image_blocks() {
        for yaml in [
            "global:\n  image:\n    repository: appwrite/cloud\n    tag: *old\n",
            "global:\n  image:\n    repository: *repository\n    tag: 1.53.30\n",
            "global:\n  image: &cloud-image\n    repository: appwrite/cloud\n    tag: 1.53.30\n",
            "global:\n  image: *cloud-image\n",
            "global:\n  image:\n    <<: *cloud-image\n    tag: 1.53.31\n",
            "global:\n  image:\n    tag: &pinned 1.53.31\n",
            "global:\n  image: {repository: appwrite/cloud, <<: *pinned}\n",
            "global:\n  image: {repository: appwrite/cloud, tag: *pinned}\n",
        ] {
            let error = scan_error(yaml);
            assert!(error.contains("global.image"), "{yaml:?}: {error}");
        }
    }

    #[test]
    fn test_image_references_rejects_values_it_cannot_read() {
        for yaml in [
            "global:\n  image:\n    repository: appwrite/cloud\n    extra:\n      tag: 8.8.8\n",
            "global:\n  image:\n    tag: |\n      1.53.31\n",
            "global:\n  image: |\n    appwrite/cloud:1.53.31\n",
            "global:\n  image:\n    - appwrite/cloud:1.53.31\n",
            "global:\n  image: [appwrite/cloud]\n",
            "global:\n  image: {repository: appwrite/cloud, tag: 1.53.31\n",
            "global:\n  image:\n    tag:\n      name: 1.53.31\n",
            "global:\n  image:\n    tag: 1.50\n",
            "global:\n  image:\n    tag: null\n",
            "global:\n  image:\n    tag: \"\"\n",
            "global: {image: {repository: appwrite/cloud, tag: 1.53.31}}\n",
            "cloud:\n  containers: [\n    {name: api, image: appwrite/cloud:1.53.31}\n  ]\n",
        ] {
            scan_error(yaml);
        }
        assert_eq!(
            image_references("global:\n  image:\n    tag: \"1.50\"\n").unwrap(),
            vec![mapping("global.image", None, Some("1.50"))]
        );
        assert!(
            image_references("global:\n  args: [\"--image-pull\", \"x\"]\n")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_image_references_strips_comments_after_plain_apostrophes() {
        let yaml = "global:\n  image:\n    repository: appwrite/cloud\n    tag: 1.0.0\n  note: don't # it's 'quoted'\n";
        assert_eq!(
            image_references(yaml).unwrap(),
            vec![mapping("global.image", Some(CLOUD_IMAGE), Some("1.0.0"))]
        );
        assert_eq!(strip_comment("  note: don't # it's"), "  note: don't ");
    }

    #[test]
    fn test_image_references_keeps_hashes_inside_quotes() {
        let yaml =
            "global:\n  image:\n    repository: \"appwrite/cloud#edge\"\n    tag: 1.0.0 # comment\n";
        assert_eq!(
            image_references(yaml).unwrap(),
            vec![mapping(
                "global.image",
                Some("appwrite/cloud#edge"),
                Some("1.0.0")
            )]
        );
    }

    #[test]
    fn test_split_image_separates_registry_ports_tags_and_digests() {
        assert_eq!(
            split_image("appwrite/cloud:1.53.30"),
            ("appwrite/cloud".to_string(), Some("1.53.30".to_string()))
        );
        assert_eq!(
            split_image("registry.local:5000/appwrite/cloud"),
            ("registry.local:5000/appwrite/cloud".to_string(), None)
        );
        assert_eq!(
            split_image("ghcr.io/a/b:0.3.0@sha256:04a7"),
            ("ghcr.io/a/b".to_string(), Some("0.3.0".to_string()))
        );
        assert!(names_image("docker.io/appwrite/cloud", CLOUD_IMAGE));
        assert!(names_image("Appwrite/Cloud", CLOUD_IMAGE));
        assert!(!names_image("appwrite/cloud-dev", CLOUD_IMAGE));
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

    async fn pinned_error(fra1: &str) -> String {
        resolver(production_with(fra1))
            .pinned_versions(&cloud_application())
            .await
            .expect_err("an override the scan cannot attribute must fail closed")
            .to_string()
    }

    #[tokio::test]
    async fn test_pinned_versions_rejects_a_tag_only_override_outside_the_default_paths() {
        let error = pinned_error("cloud:\n  image:\n    tag: 1.52.0\n").await;
        assert!(error.contains("cloud.image"), "{error}");
        assert!(error.contains("fra1.yaml"), "{error}");
    }

    #[tokio::test]
    async fn test_pinned_versions_rejects_a_scalar_application_image() {
        let error = pinned_error("cloud:\n  image: appwrite/cloud:1.52.0\n").await;
        assert!(error.contains("single image string"), "{error}");
        let error = pinned_error("cloud:\n  image: \"docker.io/appwrite/cloud:1.52.0\"\n").await;
        assert!(error.contains("single image string"), "{error}");
    }

    #[tokio::test]
    async fn test_pinned_versions_rejects_aliased_and_merged_override_tags() {
        let error = pinned_error("x-old: &old 1.52.0\nglobal:\n  image:\n    tag: *old\n").await;
        assert!(error.contains("alias, anchor or merge key"), "{error}");
        let error =
            pinned_error("x-old: &old\n  tag: 1.52.0\nglobal:\n  image:\n    <<: *old\n").await;
        assert!(error.contains("alias, anchor or merge key"), "{error}");
    }

    #[tokio::test]
    async fn test_pinned_versions_rejects_an_override_that_swaps_the_application_image() {
        let error = pinned_error(
            "global:\n  image:\n    repository: appwrite/cloud-canary\n    tag: 1.52.0\n",
        )
        .await;
        assert!(error.contains("replaces appwrite/cloud"), "{error}");
    }

    #[tokio::test]
    async fn test_pinned_versions_rejects_a_tag_only_list_item_override() {
        let error = pinned_error("cloud:\n  initContainers:\n    - name: php-fast-profile\n      image:\n        tag: 0.4.0\n").await;
        assert!(error.contains("without a repository"), "{error}");
    }

    #[tokio::test]
    async fn test_pinned_versions_counts_a_registry_qualified_application_image() {
        let versions_pinned = resolver(production_with(
            "worker:\n  image:\n    repository: docker.io/appwrite/cloud\n    tag: 1.53.32\n",
        ))
        .pinned_versions(&cloud_application())
        .await
        .unwrap();

        assert_eq!(versions_pinned, versions(&["1.53.30", "1.53.32"]));
    }

    #[tokio::test]
    async fn test_pinned_versions_inherits_the_default_tag_for_a_repository_only_override() {
        let versions_pinned = resolver(production_with(
            "global:\n  image:\n    repository: appwrite/cloud\n",
        ))
        .pinned_versions(&cloud_application())
        .await
        .unwrap();

        assert_eq!(versions_pinned, versions(&["1.53.30"]));
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

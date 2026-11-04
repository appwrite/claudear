//! Announcements for tips no release feed posts.
//!
//! `#releases` is fed by GitHub's own Discord webhook, which fires only for
//! GitHub releases. A bare tag, such as `apps/console/<version>` in the
//! appwrite monorepo, would go unannounced, so the deploy-QA poller announces
//! it itself and the QA outcome replies under that announcement.

use async_trait::async_trait;
use claudear_core::error::Result;

/// Embed footer marking a message as a deploy-QA announcement. Distinct from
/// [`crate::deploy_qa::DEPLOY_QA_SOURCE`], the footer of QA outcomes, so the
/// outcome reporter can still find an announcement as the post to reply to.
pub const ANNOUNCEMENT_FOOTER: &str = "deploy_qa announcement";

/// A bare tag's deploy, announced where a GitHub release would be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployQaAnnouncement {
    /// `[[deploy_qa.tracks]]` name.
    pub track: String,
    /// GitHub `owner/repo`.
    pub repo: String,
    /// Full tag name (e.g. `apps/console/1.2.141`).
    pub tag: String,
    /// The version the tag names, without its track prefix.
    pub version: String,
    /// Link to the tag; the stored tip's URL, so the outcome reporter's
    /// release-link match finds this announcement.
    pub tag_url: String,
    /// The track's tag before this one.
    pub previous_tag: Option<String>,
    /// The workflow run the tag push started.
    pub deploy_run: Option<DeployRun>,
    /// GitHub login of whoever cut the tag.
    pub releaser_login: Option<String>,
}

impl DeployQaAnnouncement {
    /// Compare page for the changes since [`Self::previous_tag`].
    pub fn compare_url(&self) -> Option<String> {
        self.previous_tag.as_ref().map(|previous| {
            format!(
                "https://github.com/{}/compare/{previous}...{}",
                self.repo, self.tag
            )
        })
    }
}

/// A workflow run a tag push started, such as the deploy it triggered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployRun {
    /// Display name, e.g. `Console / Production #312`.
    pub name: String,
    /// Run page URL.
    pub url: String,
}

/// Posts [`DeployQaAnnouncement`]s.
#[async_trait]
pub trait DeployQaAnnouncer: Send + Sync {
    /// Announce `announcement` and return the announcement's message id.
    ///
    /// Returns the id of an announcement already posted for the same tag
    /// instead of posting another, so a tip whose id could not be stored is
    /// never announced twice.
    async fn announce(&self, announcement: &DeployQaAnnouncement) -> Result<String>;
}

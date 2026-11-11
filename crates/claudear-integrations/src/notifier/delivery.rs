//! Who a PR notification reached.

use claudear_analysis::distribution::Digest;
use claudear_analysis::distribution::DigestGroup;
use std::collections::HashSet;

/// Whether a PR assignment ping reached its assignee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// A channel that reaches people delivered it.
    Delivered,
    /// Only channels that write to the local process output took it, so
    /// nobody was told.
    Local,
}

impl Delivery {
    /// The delivery of a message a channel accepted, given whether that
    /// channel only writes to the local process output.
    pub fn accepted(local: bool) -> Self {
        if local {
            Self::Local
        } else {
            Self::Delivered
        }
    }
}

/// The PRs of a digest whose assignee a channel that reaches people
/// delivered their digest message to.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DigestDelivery {
    pull_requests: HashSet<String>,
    developers: HashSet<String>,
}

impl DigestDelivery {
    /// The delivery of a message a channel accepted for every assignee of
    /// `digest`, given whether that channel only writes to the local process
    /// output.
    pub fn accepted(digest: &Digest, local: bool) -> Self {
        let mut delivery = Self::default();
        if !local {
            digest.groups.iter().for_each(|group| delivery.add(group));
        }
        delivery
    }

    /// Record that `group`'s assignee was sent their digest message.
    pub fn add(&mut self, group: &DigestGroup) {
        self.developers.insert(group.developer.login.clone());
        self.pull_requests.extend(
            group
                .pull_requests
                .iter()
                .map(|pull_request| pull_request.url.clone()),
        );
    }

    /// Record everything `other` delivered as delivered here too.
    pub fn merge(&mut self, other: Self) {
        self.pull_requests.extend(other.pull_requests);
        self.developers.extend(other.developers);
    }

    /// Whether the assignee of the PR at `url` was sent it.
    pub fn contains(&self, url: &str) -> bool {
        self.pull_requests.contains(url)
    }

    /// Whether nobody was sent anything.
    pub fn is_empty(&self) -> bool {
        self.developers.is_empty()
    }
}

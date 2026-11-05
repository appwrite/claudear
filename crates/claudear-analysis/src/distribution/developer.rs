//! A developer open PRs can be assigned to.

/// A developer on the roster, with the chat identities used to mention them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Developer {
    /// Canonical GitHub login.
    pub login: String,
    /// Discord user id, mentioned as `<@id>`.
    pub discord_id: Option<String>,
    /// Slack user id, mentioned as `<@id>`.
    pub slack_id: Option<String>,
}

impl Developer {
    /// A developer known only by their GitHub login.
    pub fn new(login: impl Into<String>) -> Self {
        Self {
            login: login.into(),
            discord_id: None,
            slack_id: None,
        }
    }

    /// Plain-text handle for channels without a native mention.
    pub fn handle(&self) -> String {
        format!("@{}", self.login)
    }
}

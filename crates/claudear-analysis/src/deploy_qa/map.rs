//! GitHub login → Discord user id map used for FAIL `@releaser`.

use claudear_core::error::{Error, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// One mapped Discord identity.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MappedDiscordUser {
    /// Discord username (display only).
    #[serde(default)]
    pub discord_username: Option<String>,
    /// Snowflake to ping as `<@id>`.
    pub discord_user_id: String,
    /// Verification status (`verified`, `verified_probable`, …).
    #[serde(default)]
    pub status: Option<String>,
}

/// File format matching `github-discord-map.example.json`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GitHubDiscordMap {
    /// Discord guild the ids belong to.
    #[serde(default)]
    pub guild_id: Option<String>,
    /// GitHub login → Discord user.
    #[serde(default)]
    pub by_github_login: HashMap<String, MappedDiscordUser>,
}

impl GitHubDiscordMap {
    /// Load a map from a JSON file.
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|error| {
            Error::config(format!(
                "Failed to read GitHub↔Discord map '{}': {error}",
                path.display()
            ))
        })?;
        Self::from_json_bytes(&bytes)
    }

    /// Parse the map from JSON bytes.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes)
            .map_err(|error| Error::config(format!("Invalid GitHub↔Discord map JSON: {error}")))
    }

    /// Discord mention (`<@id>`) for a GitHub login, if mapped.
    ///
    /// GitHub logins are case-insensitive, so the lookup ignores ASCII case.
    pub fn mention_for(&self, github_login: &str) -> Option<String> {
        self.by_github_login
            .get(github_login)
            .or_else(|| {
                self.by_github_login
                    .iter()
                    .find(|(login, _)| login.eq_ignore_ascii_case(github_login))
                    .map(|(_, user)| user)
            })
            .map(|user| format!("<@{}>", user.discord_user_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "guildId": "938747207446839356",
      "byGithubLogin": {
        "abnegate": {
          "discordUsername": "abnegate.",
          "discordUserId": "452316113016193024",
          "status": "verified"
        }
      }
    }"#;

    #[test]
    fn parses_example_shape_and_mentions() {
        let map = GitHubDiscordMap::from_json_bytes(SAMPLE.as_bytes()).unwrap();
        assert_eq!(map.guild_id.as_deref(), Some("938747207446839356"));
        assert_eq!(
            map.mention_for("abnegate").as_deref(),
            Some("<@452316113016193024>")
        );
        assert!(map.mention_for("unknown").is_none());
    }

    #[test]
    fn mention_lookup_ignores_login_case() {
        let map = GitHubDiscordMap::from_json_bytes(SAMPLE.as_bytes()).unwrap();
        let expected = map.mention_for("abnegate");
        assert!(expected.is_some());
        for login in ["Abnegate", "ABNEGATE", "aBnEgAtE"] {
            assert_eq!(map.mention_for(login), expected, "{login}");
        }
    }
}

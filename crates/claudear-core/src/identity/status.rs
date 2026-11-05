use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Whether a console user can sign in yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserStatus {
    Active,
    /// Created without a password; waiting for the invite to be accepted.
    Invited,
}

impl UserStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Invited => "invited",
        }
    }
}

impl fmt::Display for UserStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for UserStatus {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "active" => Ok(Self::Active),
            "invited" => Ok(Self::Invited),
            other => Err(format!("unknown user status: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_serde_and_from_str() {
        for status in [UserStatus::Active, UserStatus::Invited] {
            let serialized = serde_json::to_string(&status).expect("serialize status");
            assert_eq!(serialized, format!("\"{}\"", status.as_str()));
            let parsed: UserStatus = serde_json::from_str(&serialized).expect("parse status");
            assert_eq!(parsed, status);
            assert_eq!(status.as_str().parse::<UserStatus>(), Ok(status));
            assert_eq!(status.to_string(), status.as_str());
        }
    }

    #[test]
    fn from_str_rejects_unknown_values() {
        for value in ["", "Active", "ACTIVE", "pending", "disabled", " active"] {
            assert!(
                value.parse::<UserStatus>().is_err(),
                "{value:?} must not parse as a status"
            );
        }
    }

    #[test]
    fn serde_rejects_unknown_values() {
        assert!(serde_json::from_str::<UserStatus>("\"pending\"").is_err());
    }
}

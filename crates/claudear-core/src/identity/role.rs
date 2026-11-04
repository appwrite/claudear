use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// What a console user may do: admins manage users and the directory, viewers read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    Viewer,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Viewer => "viewer",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "admin" => Ok(Self::Admin),
            "viewer" => Ok(Self::Viewer),
            other => Err(format!("unknown role: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_serde_and_from_str() {
        for (role, name) in [(Role::Admin, "admin"), (Role::Viewer, "viewer")] {
            assert_eq!(role.as_str(), name);
            assert_eq!(role.to_string(), name);
            assert_eq!(name.parse::<Role>(), Ok(role));
            let serialized = serde_json::to_string(&role).expect("serialize role");
            assert_eq!(serialized, format!("\"{name}\""));
            let parsed: Role = serde_json::from_str(&serialized).expect("parse role");
            assert_eq!(parsed, role);
        }
    }

    #[test]
    fn from_str_rejects_unknown_values() {
        for value in ["", "Admin", "ADMIN", "owner", " viewer", "viewer "] {
            assert!(
                value.parse::<Role>().is_err(),
                "{value:?} must not parse as a role"
            );
        }
    }

    #[test]
    fn serde_rejects_unknown_values() {
        assert!(serde_json::from_str::<Role>("\"owner\"").is_err());
        assert!(serde_json::from_str::<Role>("\"Admin\"").is_err());
    }
}

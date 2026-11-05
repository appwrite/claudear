use serde::{Deserialize, Serialize};

/// Where a person's effective identities come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityOrigin {
    /// A console `user_identities` row owns the person.
    Console,
    /// A `[users.<key>]` config entry supplies the person.
    Config,
    /// Neither the console nor config knows the person.
    Unset,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_snake_case() {
        for (origin, name) in [
            (IdentityOrigin::Console, "\"console\""),
            (IdentityOrigin::Config, "\"config\""),
            (IdentityOrigin::Unset, "\"unset\""),
        ] {
            assert_eq!(serde_json::to_string(&origin).expect("serialize"), name);
            let parsed: IdentityOrigin = serde_json::from_str(name).expect("parse");
            assert_eq!(parsed, origin);
        }
    }
}

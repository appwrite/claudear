use claudear_config::config::UserConfig;
use claudear_config::users::UserRegistry;
use claudear_core::identity::{ConsoleIdentity, Identities};
use std::collections::HashMap;

/// A registry seeded with one config person, as `[users.<key>]` would be.
pub(crate) fn registry_with(key: &str, user: UserConfig) -> UserRegistry {
    UserRegistry::new(HashMap::from([(key.to_string(), user)]))
}

/// A console user that takes over the person `slug` with `identities`.
pub(crate) fn console_person(slug: &str, identities: Identities) -> ConsoleIdentity {
    ConsoleIdentity {
        user_id: 1,
        login_email: format!("{slug}@console.example.com"),
        name: slug.to_string(),
        slug: Some(slug.to_string()),
        identities,
        updated_at: "2026-11-04 10:00:00".to_string(),
    }
}

//! Keeps the shared [`UserRegistry`] in step with the console identities in storage.

use claudear_config::users::UserRegistry;
use claudear_storage::FixAttemptTracker;

/// Load console identities into `registry`; on storage failure keep the previous directory and warn.
/// Concurrent reloads apply in the order they read storage, so the newest snapshot wins.
pub fn reload(registry: &UserRegistry, store: &dyn FixAttemptTracker) -> bool {
    match registry.reload(|| store.list_user_identities()) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "Failed to load console identities; keeping the previous user directory"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claudear_config::config::UserConfig;
    use claudear_core::identity::{Identities, IdentityOrigin, Role};
    use claudear_storage::{
        DirectoryUser, DirectoryWrite, IdentityWrite, SqliteTracker, UserStore,
    };
    use std::collections::HashMap;

    #[test]
    fn reload_applies_stored_identities_to_every_clone() {
        let tracker = SqliteTracker::in_memory().expect("in-memory tracker");
        let user_id = tracker
            .create_user("amy@example.com", "unused-hash", "Amy", Role::Viewer)
            .expect("create user");
        tracker
            .save_directory(&[DirectoryWrite {
                user: DirectoryUser::Existing {
                    user_id,
                    name: None,
                    role: None,
                },
                identities: Some(IdentityWrite {
                    slug: Some("amy".to_string()),
                    identities: Identities {
                        github_usernames: vec!["amy-gh".to_string()],
                        ..Identities::default()
                    },
                }),
                invite: None,
            }])
            .expect("save identities");

        let registry = UserRegistry::new(HashMap::new());
        let clone = registry.clone();
        assert!(clone.resolve("github", "amy-gh").is_none());

        assert!(reload(&registry, &tracker));

        let resolved = clone
            .resolve("github", "amy-gh")
            .expect("resolved after reload");
        assert_eq!(resolved.slug, "amy");
        assert_eq!(clone.origin("amy"), Some(IdentityOrigin::Console));
    }

    #[test]
    fn reload_with_no_console_identities_keeps_config_people() {
        let tracker = SqliteTracker::in_memory().expect("in-memory tracker");
        let users = HashMap::from([(
            "jake".to_string(),
            UserConfig {
                github_usernames: vec!["jakebarnby".to_string()],
                ..UserConfig::default()
            },
        )]);
        let registry = UserRegistry::new(users);

        assert!(reload(&registry, &tracker));

        assert_eq!(
            registry
                .resolve("github", "jakebarnby")
                .map(|resolved| resolved.slug),
            Some("jake".to_string())
        );
    }
}

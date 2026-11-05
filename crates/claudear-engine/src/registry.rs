//! Keeps the shared [`UserRegistry`] in step with the console identities in storage.

use claudear_config::users::UserRegistry;
use claudear_storage::FixAttemptTracker;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::MissedTickBehavior;

/// The shortest refresh period, so a zero interval neither panics nor spins.
const MINIMUM_REFRESH_PERIOD: Duration = Duration::from_secs(1);

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

/// Reload `registry` from `store` every `period` (at least a second) until the task is
/// dropped, so a process without a housekeeping loop sees identities that other
/// processes write to the same database.
pub async fn refresh_every(
    registry: UserRegistry,
    store: Arc<dyn FixAttemptTracker>,
    period: Duration,
) {
    let mut timer = tokio::time::interval(period.max(MINIMUM_REFRESH_PERIOD));
    timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    timer.tick().await;
    loop {
        timer.tick().await;
        reload(&registry, store.as_ref());
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

    fn save_github_handle(tracker: &dyn FixAttemptTracker, user_id: i64, handle: &str) {
        tracker
            .save_directory(&[DirectoryWrite {
                user: DirectoryUser::Existing {
                    user_id,
                    name: None,
                    role: None,
                },
                identities: Some(IdentityWrite {
                    slug: None,
                    identities: Identities {
                        github_usernames: vec![handle.to_string()],
                        ..Identities::default()
                    },
                }),
                invite: None,
            }])
            .expect("save identities");
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_every_picks_up_identities_written_by_another_process() {
        const PERIOD: Duration = Duration::from_secs(30);
        let tracker = Arc::new(SqliteTracker::in_memory().expect("in-memory tracker"));
        let user_id = tracker
            .create_user("amy@example.com", "unused-hash", "Amy", Role::Viewer)
            .expect("create user");
        let registry = UserRegistry::new(HashMap::new());
        let refresh = tokio::spawn(refresh_every(registry.clone(), tracker.clone(), PERIOD));
        tokio::task::yield_now().await;

        save_github_handle(tracker.as_ref(), user_id, "amy-gh");
        tokio::time::sleep(PERIOD - Duration::from_millis(1)).await;
        assert!(registry.resolve("github", "amy-gh").is_none());

        tokio::time::sleep(Duration::from_millis(2)).await;
        assert_eq!(
            registry
                .resolve("github", "amy-gh")
                .map(|person| person.slug),
            Some(format!("console-{user_id}"))
        );

        save_github_handle(tracker.as_ref(), user_id, "amy-renamed");
        tokio::time::sleep(PERIOD).await;
        assert!(registry.resolve("github", "amy-gh").is_none());
        assert!(registry.resolve("github", "amy-renamed").is_some());

        refresh.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_every_treats_a_zero_period_as_the_minimum() {
        let tracker = Arc::new(SqliteTracker::in_memory().expect("in-memory tracker"));
        let user_id = tracker
            .create_user("amy@example.com", "unused-hash", "Amy", Role::Viewer)
            .expect("create user");
        let registry = UserRegistry::new(HashMap::new());
        let refresh = tokio::spawn(refresh_every(
            registry.clone(),
            tracker.clone(),
            Duration::ZERO,
        ));
        tokio::task::yield_now().await;

        save_github_handle(tracker.as_ref(), user_id, "amy-gh");
        tokio::time::sleep(MINIMUM_REFRESH_PERIOD - Duration::from_millis(1)).await;
        assert!(registry.resolve("github", "amy-gh").is_none());

        tokio::time::sleep(Duration::from_millis(2)).await;
        assert!(registry.resolve("github", "amy-gh").is_some());

        refresh.abort();
    }

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

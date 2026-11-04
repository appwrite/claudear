//! Sliding-window rate limits for the dashboard API, owned by one router's state.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Most keys one limiter tracks; past it the least recently used keys are evicted so a
/// distributed attack cannot grow the map without bound.
const CAPACITY: usize = 10_000;

/// Login attempts allowed per email per window.
pub(crate) const LOGIN_ATTEMPTS: usize = 10;

/// Requests allowed per client address per window, for logins, invites and Bearer
/// failures; more generous than per email so shared addresses keep working.
pub(crate) const ADDRESS_ATTEMPTS: usize = 100;

/// Sensitive API writes (profile, avatar, config, user management) allowed per user per
/// window.
pub(crate) const API_REQUESTS: usize = 30;

const LOGIN_WINDOW: Duration = Duration::from_secs(300);
const ADDRESS_WINDOW: Duration = Duration::from_secs(300);
const API_WINDOW: Duration = Duration::from_secs(60);

/// A sliding-window counter per key.
pub(crate) struct Limiter<Key> {
    name: &'static str,
    attempts: usize,
    window: Duration,
    entries: Mutex<HashMap<Key, Vec<Instant>>>,
}

impl<Key: Hash + Eq + Clone> Limiter<Key> {
    fn new(name: &'static str, attempts: usize, window: Duration) -> Self {
        Self {
            name,
            attempts,
            window,
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn entries(&self) -> MutexGuard<'_, HashMap<Key, Vec<Instant>>> {
        self.entries.lock().unwrap_or_else(|poisoned| {
            tracing::warn!(
                limiter = self.name,
                "Rate limiter mutex was poisoned, recovering"
            );
            poisoned.into_inner()
        })
    }

    /// Spend one attempt for `key`; false when its window is already full.
    pub(crate) fn allow<Query>(&self, key: &Query) -> bool
    where
        Key: Borrow<Query>,
        Query: Hash + Eq + ToOwned<Owned = Key> + ?Sized,
    {
        let now = Instant::now();
        let mut entries = self.entries();
        let attempts = entries.entry(key.to_owned()).or_default();
        attempts.retain(|attempt| now.duration_since(*attempt) < self.window);
        if attempts.len() >= self.attempts {
            return false;
        }
        attempts.push(now);

        entries.retain(|_, attempts| {
            attempts
                .iter()
                .any(|attempt| now.duration_since(*attempt) < self.window)
        });
        if entries.len() > CAPACITY {
            let mut recent: Vec<(Key, Instant)> = entries
                .iter()
                .filter_map(|(key, attempts)| attempts.last().map(|last| (key.clone(), *last)))
                .collect();
            recent.sort_by_key(|(_, last)| *last);
            let excess = entries.len() - CAPACITY;
            for (stale, _) in recent.into_iter().take(excess) {
                entries.remove::<Key>(&stale);
            }
        }
        true
    }

    /// Whether `key` has no attempts left, without spending one.
    pub(crate) fn exhausted<Query>(&self, key: &Query) -> bool
    where
        Key: Borrow<Query>,
        Query: Hash + Eq + ?Sized,
    {
        self.entries().get(key).is_some_and(|attempts| {
            attempts
                .iter()
                .filter(|attempt| attempt.elapsed() < self.window)
                .count()
                >= self.attempts
        })
    }
}

/// Every rate limit one API router enforces. The daemon builds one router per server, so
/// these are process-wide in production while each test router starts empty.
pub(crate) struct RateLimiters {
    /// Login and password-change attempts, keyed by login email.
    pub login: Limiter<String>,
    /// Login attempts, keyed by client address.
    pub address: Limiter<String>,
    /// Invite inspect and accept attempts, keyed by client address.
    pub invite: Limiter<String>,
    /// Failed Bearer token lookups, keyed by client address.
    pub bearer: Limiter<String>,
    /// Sensitive API writes, keyed by user id.
    pub api: Limiter<i64>,
}

impl Default for RateLimiters {
    fn default() -> Self {
        Self {
            login: Limiter::new("login", LOGIN_ATTEMPTS, LOGIN_WINDOW),
            address: Limiter::new("address", ADDRESS_ATTEMPTS, ADDRESS_WINDOW),
            invite: Limiter::new("invite", ADDRESS_ATTEMPTS, ADDRESS_WINDOW),
            bearer: Limiter::new("bearer", ADDRESS_ATTEMPTS, ADDRESS_WINDOW),
            api: Limiter::new("api", API_REQUESTS, API_WINDOW),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_the_attempt_after_the_limit() {
        let limiters = RateLimiters::default();
        for attempt in 0..LOGIN_ATTEMPTS {
            assert!(
                limiters.login.allow("user@example.com"),
                "attempt {} should be allowed",
                attempt + 1
            );
        }
        assert!(
            !limiters.login.allow("user@example.com"),
            "attempt {} should be rejected",
            LOGIN_ATTEMPTS + 1
        );
    }

    #[test]
    fn keys_are_counted_independently() {
        let limiters = RateLimiters::default();
        for _ in 0..LOGIN_ATTEMPTS {
            limiters.login.allow("a@example.com");
        }
        assert!(!limiters.login.allow("a@example.com"));
        assert!(limiters.login.allow("b@example.com"));
        for _ in 0..ADDRESS_ATTEMPTS {
            limiters.address.allow("10.0.0.1");
        }
        assert!(!limiters.address.allow("10.0.0.1"));
        assert!(limiters.address.allow("10.0.0.2"));
    }

    #[test]
    fn limiters_are_counted_independently() {
        let limiters = RateLimiters::default();
        for _ in 0..ADDRESS_ATTEMPTS {
            limiters.address.allow("10.0.0.1");
        }
        assert!(limiters.invite.allow("10.0.0.1"));
        assert!(limiters.bearer.allow("10.0.0.1"));
    }

    #[test]
    fn api_requests_are_limited_per_user() {
        let limiters = RateLimiters::default();
        for _ in 0..API_REQUESTS {
            assert!(limiters.api.allow(&7));
        }
        assert!(!limiters.api.allow(&7));
        assert!(limiters.api.allow(&8));
    }

    #[test]
    fn exhausted_does_not_spend_an_attempt() {
        let limiters = RateLimiters::default();
        for _ in 0..ADDRESS_ATTEMPTS - 1 {
            limiters.bearer.allow("10.0.0.1");
        }
        assert!(!limiters.bearer.exhausted("10.0.0.1"));
        assert!(!limiters.bearer.exhausted("10.0.0.1"));
        assert!(limiters.bearer.allow("10.0.0.1"));
        assert!(limiters.bearer.exhausted("10.0.0.1"));
        assert!(!limiters.bearer.exhausted("10.0.0.2"));
    }

    #[test]
    fn separate_instances_share_nothing() {
        let first = RateLimiters::default();
        for _ in 0..API_REQUESTS {
            first.api.allow(&1);
        }
        assert!(!first.api.allow(&1));
        assert!(RateLimiters::default().api.allow(&1));
    }
}

use super::assessment::Assessment;
use claudear_core::security::SecurityOutcome;
use regex_lite::{Captures, Regex};
use std::sync::LazyLock;

pub(crate) const REDACTED: &str = "[redacted]";

const MAX_PUBLIC_REASON_CHARS: usize = 600;
const PUBLIC_URL_PREFIX: &str = "https://appwrite.io/";
const URL_MARKERS: &[&str] = &["http://", "https://", "www."];
const MIN_ENCODED_SECRET_CHARS: usize = 33;

const NOT_VULNERABILITY_FALLBACK: &str = "Based on our review, the behaviour you described works as intended and does not give access beyond what is already permitted.";
const OUT_OF_SCOPE_FALLBACK: &str =
    "It matches one of the categories our program lists as out of scope.";

static SECRET_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?s)-----BEGIN [A-Z0-9 ]+-----.*?-----END [A-Z0-9 ]+-----",
        r"(?s)-----BEGIN [A-Z0-9 ]+-----.*",
        r"\b(?:gh[pousr]_[A-Za-z0-9]{16,}|github_pat_[A-Za-z0-9_]{16,})",
        r"\bsk[-_][A-Za-z0-9_-]{16,}",
        r"\bxox[abposr]-[A-Za-z0-9-]{10,}",
        r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
        r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
        r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b",
        r"(?i)\bhttps?://[A-Za-z0-9.-]*helpscout\.net\S*",
        r"(?i)\bhelp ?scout\b",
        r"\bHS-[0-9]+\b",
        r"(?i)\bappwrite-labs(?:/[A-Za-z0-9._-]+)*",
        r"(?i)\blocalhost\b(?::[0-9]+)?",
        r"(?i)\b(?:[A-Za-z0-9-]+\.)+(?:internal|svc|local)\b(?::[0-9]+)?",
        r"\b(?:10\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}|172\.(?:1[6-9]|2[0-9]|3[01])\.[0-9]{1,3}\.[0-9]{1,3}|192\.168\.[0-9]{1,3}\.[0-9]{1,3}|127\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}|169\.254\.[0-9]{1,3}\.[0-9]{1,3}|0\.0\.0\.0)\b(?::[0-9]+)?",
        r"\[?::1\]?(?::[0-9]+)?\b",
        r"(?i)\bfe80::[0-9a-f:%]*",
        r"\b[A-Za-z]:\\\S+",
        r"\b[0-9a-fA-F]{33,}\b",
    ]
    .iter()
    .map(|pattern| Regex::new(pattern).expect("valid scrub pattern"))
    .collect()
});

static ABSOLUTE_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(^|[^A-Za-z0-9._/-])/(?:home|Users|root|usr|var|etc|opt|tmp|private|srv|mnt|app|workspace|proc|run|Library|Volumes|storage|data)\b(?:/[A-Za-z0-9._@+-]+)*/?",
    )
    .expect("valid absolute path pattern")
});

static ENCODED_RUN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9+/_-]{33,}={0,2}").expect("valid encoded run pattern"));

/// Redact secret-like tokens, internal hosts and addresses, private
/// repository references, ticket references, email addresses and absolute
/// file paths, so the text is safe to store, notify or hand to a fix agent.
pub fn scrub_internal(text: &str) -> String {
    let mut scrubbed = text.to_string();
    for pattern in SECRET_PATTERNS.iter() {
        scrubbed = pattern.replace_all(&scrubbed, REDACTED).into_owned();
    }
    scrubbed = ABSOLUTE_PATH
        .replace_all(&scrubbed, |captures: &Captures<'_>| {
            format!("{}{REDACTED}", &captures[1])
        })
        .into_owned();
    ENCODED_RUN
        .replace_all(&scrubbed, |captures: &Captures<'_>| {
            let run = &captures[0];
            if looks_encoded(run) {
                REDACTED.to_string()
            } else {
                run.to_string()
            }
        })
        .into_owned()
}

fn looks_encoded(run: &str) -> bool {
    run.chars().count() >= MIN_ENCODED_SECRET_CHARS
        && run.chars().any(|character| character.is_ascii_digit())
        && run.chars().any(|character| character.is_ascii_uppercase())
        && run.chars().any(|character| character.is_ascii_lowercase())
}

/// The reporter-facing reason: scrubbed, without any sentence that lost a
/// redacted value or links anywhere but appwrite.io, capped at 600 chars, and
/// a fixed sentence for the outcome when nothing safe is left.
pub(crate) fn public_reason(raw: &str, outcome: SecurityOutcome) -> String {
    let scrubbed = without_dashes(&scrub_internal(raw));
    let mut kept = String::new();
    for sentence in sentences(&scrubbed) {
        if sentence.contains(REDACTED) || links_outside_appwrite(sentence) {
            continue;
        }
        let separator = usize::from(!kept.is_empty());
        if kept.chars().count() + separator + sentence.chars().count() > MAX_PUBLIC_REASON_CHARS {
            break;
        }
        if !kept.is_empty() {
            kept.push(' ');
        }
        kept.push_str(sentence);
    }
    if kept.is_empty() {
        fallback_reason(outcome).to_string()
    } else {
        kept
    }
}

fn without_dashes(text: &str) -> String {
    text.replace(" \u{2014} ", ", ")
        .replace(" \u{2013} ", ", ")
        .replace(['\u{2014}', '\u{2013}'], ", ")
}

fn fallback_reason(outcome: SecurityOutcome) -> &'static str {
    match outcome {
        SecurityOutcome::NotVulnerability => NOT_VULNERABILITY_FALLBACK,
        SecurityOutcome::OutOfScope => OUT_OF_SCOPE_FALLBACK,
        _ => "",
    }
}

fn sentences(text: &str) -> impl Iterator<Item = &str> {
    let mut boundaries = Vec::new();
    let mut start = 0;
    let mut characters = text.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        let next_is_break = characters
            .peek()
            .is_none_or(|(_, next)| next.is_whitespace());
        let ends = character == '\n' || (matches!(character, '.' | '!' | '?') && next_is_break);
        if ends {
            let end = index + character.len_utf8();
            boundaries.push(&text[start..end]);
            start = end;
        }
    }
    boundaries.push(&text[start..]);
    boundaries
        .into_iter()
        .map(str::trim)
        .filter(|sentence| !sentence.is_empty())
}

fn links_outside_appwrite(sentence: &str) -> bool {
    sentence.split_whitespace().any(|word| {
        let word = word.trim_start_matches(['(', '<', '"', '\'']);
        URL_MARKERS.iter().any(|marker| word.contains(marker))
            && !word.starts_with(PUBLIC_URL_PREFIX)
    })
}

/// Scrub every free-text field an agent produced, before any of it is
/// stored, sent to the fix agent, notified or shown.
pub(crate) fn scrub_assessment(assessment: &mut Assessment) {
    for field in [
        &mut assessment.title,
        &mut assessment.summary,
        &mut assessment.root_cause,
        &mut assessment.suggested_fix,
        &mut assessment.evidence,
        &mut assessment.public_reason,
    ] {
        *field = scrub_internal(field);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLANTED: &[&str] = &[
        "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
        "github_pat_11ABCDEFG0123456789_abcdefghijklmnop",
        "sk-proj-abcdefghijklmnopqrstuvwx",
        "xoxb-1234567890-abcdefghij",
        "AKIAIOSFODNN7EXAMPLE",
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
        "0123456789abcdef0123456789abcdef0123",
        "QWxhZGRpbjpvcGVuIHNlc2FtZTEyMzQ1Njc4OTBhYmNkZWY",
        "researcher@example.com",
        "https://secure.helpscout.net/conversation/123",
        "HS-4521",
        "appwrite-labs/cloud",
        "localhost:8080",
        "redis.default.svc.cluster.local",
        "db.internal",
        "printer.local",
        "10.0.3.7",
        "172.20.1.1",
        "192.168.1.10",
        "127.0.0.1",
        "169.254.169.254",
        "[::1]:9000",
        "fe80::1ff:fe23:4567:890a",
        "/home/claudear/workspace/cloud/app/init.php",
        "/etc/hosts",
        "/Users/someone/.ssh/id_rsa",
        "C:\\Users\\someone\\secrets.txt",
    ];

    #[test]
    fn every_planted_secret_host_and_path_is_redacted() {
        for planted in PLANTED {
            let scrubbed = scrub_internal(&format!("see {planted} for details"));
            assert!(
                !scrubbed.contains(planted),
                "{planted} survived scrubbing: {scrubbed}"
            );
            assert!(scrubbed.contains(REDACTED), "{planted} was not redacted");
        }
    }

    #[test]
    fn pem_blocks_are_redacted_whole_even_when_unterminated() {
        let block =
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\n-----END RSA PRIVATE KEY-----";
        assert_eq!(
            scrub_internal(&format!("key {block} end")),
            "key [redacted] end"
        );
        let unterminated = "start -----BEGIN PRIVATE KEY-----\nMIIEowIBAAKCAQEA and more";
        assert_eq!(scrub_internal(unterminated), "start [redacted]");
    }

    #[test]
    fn ordinary_technical_text_is_kept() {
        let text = "The POST /v1/account/sessions endpoint in src/Appwrite/Auth/Validator/Password.php accepts https://appwrite.io/docs links and the appwrite/appwrite repository.";
        assert_eq!(scrub_internal(text), text);
    }

    #[test]
    fn url_paths_after_a_host_are_not_treated_as_file_paths() {
        let text = "https://cloud.appwrite.io/home/projects is the console";
        assert_eq!(scrub_internal(text), text);
    }

    #[test]
    fn public_reason_drops_sentences_with_redactions_or_foreign_links() {
        let raw = "Sessions are scoped per project. The check lives on db.internal. See https://evil.example/poc for more. Read https://appwrite.io/docs/advanced/security for details.";
        assert_eq!(
            public_reason(raw, SecurityOutcome::NotVulnerability),
            "Sessions are scoped per project. Read https://appwrite.io/docs/advanced/security for details."
        );
    }

    #[test]
    fn public_reason_falls_back_per_outcome_when_nothing_is_left() {
        assert_eq!(
            public_reason(
                "Token ghp_abcdefghijklmnopqrstuvwxyz0123456789.",
                SecurityOutcome::NotVulnerability
            ),
            NOT_VULNERABILITY_FALLBACK
        );
        assert_eq!(
            public_reason("", SecurityOutcome::OutOfScope),
            OUT_OF_SCOPE_FALLBACK
        );
        assert_eq!(public_reason("   ", SecurityOutcome::NotReproducible), "");
    }

    #[test]
    fn public_reason_is_capped_at_whole_sentences() {
        let sentence = "This sentence is exactly fifty characters long ok.";
        let raw = vec![sentence; 20].join(" ");
        let reason = public_reason(&raw, SecurityOutcome::OutOfScope);
        assert!(reason.chars().count() <= MAX_PUBLIC_REASON_CHARS);
        assert!(reason.ends_with('.'));
        assert_eq!(reason.matches(sentence).count(), 11);
    }

    #[test]
    fn public_reason_replaces_dashes() {
        assert_eq!(
            public_reason(
                "It is expected \u{2014} by design.",
                SecurityOutcome::NotVulnerability
            ),
            "It is expected, by design."
        );
    }

    #[test]
    fn scrub_assessment_covers_every_free_text_field() {
        let token = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let host = "billing.svc.cluster.local";
        let planted = format!("{token} on {host}");
        let mut assessment = Assessment {
            title: planted.clone(),
            summary: planted.clone(),
            root_cause: planted.clone(),
            suggested_fix: planted.clone(),
            evidence: planted.clone(),
            public_reason: planted.clone(),
            ..Assessment::default()
        };
        scrub_assessment(&mut assessment);
        for field in [
            &assessment.title,
            &assessment.summary,
            &assessment.root_cause,
            &assessment.suggested_fix,
            &assessment.evidence,
            &assessment.public_reason,
        ] {
            assert!(!field.contains(token) && !field.contains(host), "{field}");
            assert_eq!(field, "[redacted] on [redacted]");
        }
    }
}

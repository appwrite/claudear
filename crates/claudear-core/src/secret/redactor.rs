//! Credential redaction for untrusted text leaving Claudear.

use super::{redact_secrets, SecretValue, REDACTED};
use regex_lite::{Captures, NoExpand, Regex};
use std::sync::LazyLock;

/// Shortest value masked verbatim; shorter ones would mask ordinary words.
const MINIMUM_SECRET_LENGTH: usize = 8;

/// Headers whose values are credentials, matched ignoring case.
const CREDENTIAL_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "x-appwrite-key",
    "x-appwrite-dev-key",
    "x-appwrite-jwt",
    "x-appwrite-session",
    "x-fallback-cookies",
];

/// Fragments that mark a variable or field name as a credential, once it is
/// uppercased with `-` and `.` read as `_`.
const SECRET_NAME_MARKERS: &[&str] = &[
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "API_KEY",
    "APIKEY",
    "PRIVATE_KEY",
    "PRIVATEKEY",
    "AUTH",
    "JWT",
    "COOKIE",
];

/// Name endings that mark a credential, as in `ENCRYPTION_KEY` or `DB_PASS`.
const SECRET_NAME_SUFFIXES: &[&str] = &["_KEY", "_PASS"];

/// Name segments that contain a marker without naming a credential, as in
/// `GIT_AUTHOR_NAME`.
const NON_SECRET_NAME_SEGMENTS: &[&str] = &["AUTHOR", "AUTHORS"];

/// Characters found in tokens but not in words.
const TOKEN_SYMBOLS: &str = "_.~+/=";

/// Ends an unquoted header value that sits in a markdown table cell.
const TABLE_CELL_BORDER: char = '|';

/// Markdown emphasis that may wrap a header's name or value.
const EMPHASIS: char = '*';

/// Capture group holding the text to mask.
const SECRET_GROUP: &str = "secret";

/// Capture group holding a variable or field name.
const NAME_GROUP: &str = "name";

/// Capture group holding a quote before a header's name.
const LEAD_QUOTE_GROUP: &str = "lead";

/// Capture group holding a quote that opens a header's value.
const OPEN_QUOTE_GROUP: &str = "open";

/// A PEM block through its `-----END` line, or to the end of the text when
/// the block was cut short.
static PEM_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)-----BEGIN [A-Z0-9 ]+-----.*?(?:-----END [A-Z0-9 ]+-----|\z)")
        .expect("PEM block pattern is valid")
});

/// A credential header's name and separator, with any quote opening the
/// header before its name or opening the value after it. The name may be
/// quoted or emphasised, and a markdown table cell border may stand in for
/// the colon.
static CREDENTIAL_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?i)(?<{LEAD_QUOTE_GROUP}>["'`]?)\b(?:{})["'`{EMPHASIS}]*[ \t]*[:{TABLE_CELL_BORDER}][ \t{EMPHASIS}]*(?<{OPEN_QUOTE_GROUP}>["'`]?)"#,
        CREDENTIAL_HEADERS.join("|")
    ))
    .expect("credential header pattern is valid")
});

/// A `Bearer` token: RFC 6750 token characters, never ending in a sentence's
/// full stop.
static BEARER_CREDENTIALS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)\bbearer[ \t]+(?<{SECRET_GROUP}>[A-Za-z0-9._~+/-]*[A-Za-z0-9_~+/-]=*)"
    ))
    .expect("Bearer pattern is valid")
});

/// `Basic` credentials: base64 of `user:password`.
static BASIC_CREDENTIALS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)\bbasic[ \t]+(?<{SECRET_GROUP}>[A-Za-z0-9+/]+=*)"
    ))
    .expect("Basic pattern is valid")
});

/// The password in a URL's userinfo, as in `postgres://user:password@host`.
static URL_PASSWORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)\b[a-z][a-z0-9+.-]*://[^\s:/@]*:(?<{SECRET_GROUP}>[^\s/@]+)@"
    ))
    .expect("URL password pattern is valid")
});

/// `NAME=value`, as `env`, shells and query strings write it.
static VARIABLE_ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"\b(?<{NAME_GROUP}>[A-Za-z_][A-Za-z0-9_.-]*)=(?<{SECRET_GROUP}>"[^"\r\n]+"|'[^'\r\n]+'|[^\s"'&;,]+)"#
    ))
    .expect("variable assignment pattern is valid")
});

/// `"name": "value"`, as JSON writes a string field.
static JSON_FIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#""(?<{NAME_GROUP}>[A-Za-z_$][A-Za-z0-9_.$-]*)"[ \t]*:[ \t]*"(?<{SECRET_GROUP}>(?:[^"\\\r\n]|\\.)+)""#
    ))
    .expect("JSON field pattern is valid")
});

/// A JSON Web Token: base64url JSON header and payload, then the signature.
static JWT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\beyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]*")
        .expect("JWT pattern is valid")
});

/// Masks credentials in untrusted text, such as an agent's report, before it
/// leaves Claudear.
///
/// Beyond the credential shapes every redactor knows, each redactor masks the
/// exact secrets it was built with, so a configured token is caught whatever
/// it looks like. Those are held as [`SecretValue`]s: redacted in `Debug`
/// and zeroized on drop.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    secrets: Vec<SecretValue>,
}

impl Redactor {
    /// A redactor that also masks each of `secrets` verbatim. Values are
    /// trimmed, and those shorter than eight characters are skipped because
    /// they would mask ordinary words.
    pub fn new(secrets: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self::default().with_secrets(secrets)
    }

    /// Also mask each of `secrets` verbatim, as [`Redactor::new`] does.
    pub fn with_secrets(mut self, secrets: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        self.secrets.extend(
            secrets
                .into_iter()
                .map(|secret| secret.as_ref().trim().to_string())
                .filter(|secret| secret.chars().count() >= MINIMUM_SECRET_LENGTH)
                .map(SecretValue::new),
        );
        self.secrets.sort_by(|left, right| {
            right
                .expose()
                .len()
                .cmp(&left.expose().len())
                .then_with(|| left.expose().cmp(right.expose()))
        });
        self.secrets.dedup();
        self
    }

    /// Also mask the value of every variable in `variables` whose name marks
    /// it as a credential, such as `GITHUB_TOKEN`, `DB_PASSWORD` or
    /// `ENCRYPTION_KEY`.
    pub fn with_variables<I, N, V>(self, variables: I) -> Self
    where
        I: IntoIterator<Item = (N, V)>,
        N: AsRef<str>,
        V: AsRef<str>,
    {
        self.with_secrets(
            variables
                .into_iter()
                .filter(|(name, _)| is_secret_name(name.as_ref()))
                .map(|(_, value)| value),
        )
    }

    /// Replace every credential in `text` with [`REDACTED`]: the known
    /// secrets, PEM blocks, credential header values, `Bearer` and `Basic`
    /// credentials, URL passwords, the values of secret-named `NAME=value`
    /// assignments and JSON fields, JWTs, and tokens with a known prefix.
    pub fn redact(&self, text: &str) -> String {
        let text = self.redact_known(text);
        let text = PEM_BLOCK.replace_all(&text, NoExpand(REDACTED));
        let text = redact_credential_headers(&text);
        let text = redact_matches(&text, &BEARER_CREDENTIALS, is_credential);
        let text = redact_matches(&text, &BASIC_CREDENTIALS, is_credential);
        let text = redact_matches(&text, &URL_PASSWORD, |_, _| true);
        let text = redact_matches(&text, &VARIABLE_ASSIGNMENT, has_secret_name);
        let text = redact_matches(&text, &JSON_FIELD, has_secret_name);
        let text = JWT.replace_all(&text, NoExpand(REDACTED));
        redact_secrets(&text)
    }

    /// Longest first, so a secret that contains another is masked whole.
    fn redact_known(&self, text: &str) -> String {
        self.secrets
            .iter()
            .map(SecretValue::expose)
            .fold(text.to_string(), |text, secret| {
                if text.contains(secret) {
                    text.replace(secret, REDACTED)
                } else {
                    text
                }
            })
    }
}

/// Mask each credential header's value. A value opened by a quote, or in a
/// header quoted as a whole like `-H "Authorization: …"`, ends at that quote;
/// any other value runs to the end of the line, as in an HTTP message, or of
/// its markdown table cell.
fn redact_credential_headers(text: &str) -> String {
    let mut redacted = String::with_capacity(text.len());
    let mut cursor = 0;
    for header in CREDENTIAL_HEADER.captures_iter(text) {
        let prefix = header.get(0).expect("every match has a whole-match group");
        if prefix.start() < cursor {
            continue;
        }
        let quote = |group| {
            header
                .name(group)
                .and_then(|quote| quote.as_str().chars().next())
        };
        let terminator = quote(OPEN_QUOTE_GROUP)
            .or_else(|| quote(LEAD_QUOTE_GROUP))
            .unwrap_or(TABLE_CELL_BORDER);
        let value = &text[prefix.end()..];
        let end = value.find([terminator, '\r', '\n']).unwrap_or(value.len());
        let length = value[..end]
            .trim_end_matches(|character: char| character.is_whitespace() || character == EMPHASIS)
            .len();
        if length == 0 {
            continue;
        }
        redacted.push_str(&text[cursor..prefix.end()]);
        redacted.push_str(REDACTED);
        cursor = prefix.end() + length;
    }
    redacted.push_str(&text[cursor..]);
    redacted
}

/// Replace the secret group of each `pattern` match that `is_secret` accepts,
/// given the match and the secret, with [`REDACTED`].
fn redact_matches(
    text: &str,
    pattern: &Regex,
    is_secret: impl Fn(&Captures<'_>, &str) -> bool,
) -> String {
    pattern
        .replace_all(text, |captures: &Captures<'_>| {
            let whole = captures
                .get(0)
                .expect("every match has a whole-match group");
            let matched = whole.as_str();
            match captures
                .name(SECRET_GROUP)
                .filter(|secret| is_secret(captures, secret.as_str()))
            {
                Some(secret) => format!(
                    "{}{REDACTED}{}",
                    &matched[..secret.start() - whole.start()],
                    &matched[secret.end() - whole.start()..]
                ),
                None => matched.to_string(),
            }
        })
        .into_owned()
}

fn is_credential(_: &Captures<'_>, token: &str) -> bool {
    looks_like_credential(token)
}

fn has_secret_name(assignment: &Captures<'_>, _: &str) -> bool {
    assignment
        .name(NAME_GROUP)
        .is_some_and(|name| is_secret_name(name.as_str()))
}

/// Whether `token` is shaped like a credential rather than a word: at least
/// eight characters, with a digit, a token symbol or a capital after the
/// first character.
fn looks_like_credential(token: &str) -> bool {
    token.chars().count() >= MINIMUM_SECRET_LENGTH
        && (token.contains(|character: char| {
            character.is_ascii_digit() || TOKEN_SYMBOLS.contains(character)
        }) || token
            .chars()
            .skip(1)
            .any(|character| character.is_ascii_uppercase()))
}

/// Whether a variable or field called `name` holds a credential by naming
/// convention: `GITHUB_TOKEN`, `DB_PASSWORD`, `apiKey` and `ENCRYPTION_KEY`
/// do; `GIT_AUTHOR_NAME` and `twoWayKey` do not.
fn is_secret_name(name: &str) -> bool {
    let name = name
        .to_ascii_uppercase()
        .replace(['-', '.'], "_")
        .split('_')
        .filter(|segment| !NON_SECRET_NAME_SEGMENTS.contains(segment))
        .collect::<Vec<_>>()
        .join("_");
    SECRET_NAME_MARKERS
        .iter()
        .any(|marker| name.contains(marker))
        || SECRET_NAME_SUFFIXES
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KNOWN_SECRET: &str = "configured-bot-token-0001";

    fn redact(text: &str) -> String {
        Redactor::default().redact(text)
    }

    #[test]
    fn credential_header_values_are_redacted_ignoring_case() {
        let headers = [
            "Authorization: Bearer abc123",
            "authorization: token abc123",
            "PROXY-AUTHORIZATION: Basic dXNlcjpwYXNz",
            "Cookie: a_session_console=abc123; theme=dark",
            "set-cookie: a_session=abc123; Path=/; HttpOnly",
            "X-Appwrite-Key: standard_abc123",
            "x-appwrite-session: abc123",
            "X-API-KEY: abc123",
            "X-Appwrite-JWT: abc123",
            "X-Appwrite-Dev-Key: abc123",
            r#"x-fallback-cookies: {"a_session":"abc123"}"#,
        ];
        for header in headers {
            let name = header.split(':').next().unwrap_or_default();
            assert_eq!(redact(header), format!("{name}: {REDACTED}"), "{header}");
        }
    }

    #[test]
    fn unquoted_header_values_run_to_the_end_of_the_line_or_table_cell() {
        let cases = [
            (
                "> Authorization: Bearer abc123 (expired) → 401\r\n> Accept: */*",
                format!("> Authorization: {REDACTED}\r\n> Accept: */*"),
            ),
            (
                "Cookie: a_session=abc123; Authorization: Bearer abc123",
                format!("Cookie: {REDACTED}"),
            ),
            (
                "| X-Appwrite-Key | standard_abc123 | 401 |",
                format!("| X-Appwrite-Key | {REDACTED} | 401 |"),
            ),
            (
                "| #42 | Authorization: Bearer abc123 | LIVE FAIL |",
                format!("| #42 | Authorization: {REDACTED} | LIVE FAIL |"),
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(redact(text), expected, "{text}");
        }
    }

    #[test]
    fn emphasised_header_names_and_values_are_redacted() {
        let cases = [
            (
                "**X-Appwrite-Key**: standard_abc123",
                format!("**X-Appwrite-Key**: {REDACTED}"),
            ),
            (
                "**Authorization:** Bearer abc123",
                format!("**Authorization:** {REDACTED}"),
            ),
            (
                "Authorization: **Bearer abc123**",
                format!("Authorization: **{REDACTED}**"),
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(redact(text), expected, "{text}");
        }
    }

    #[test]
    fn quoted_header_values_end_at_their_closing_quote() {
        let cases = [
            (
                r#"curl -H "Authorization: Bearer abc123" -H 'X-Appwrite-Project: qa' https://cloud.appwrite.io/v1/account"#,
                format!(
                    r#"curl -H "Authorization: {REDACTED}" -H 'X-Appwrite-Project: qa' https://cloud.appwrite.io/v1/account"#
                ),
            ),
            (
                r#"curl -H "Authorization: Bearer abc123" -H "X-Appwrite-Key: key123" https://cloud.appwrite.io"#,
                format!(
                    r#"curl -H "Authorization: {REDACTED}" -H "X-Appwrite-Key: {REDACTED}" https://cloud.appwrite.io"#
                ),
            ),
            (
                r#"{"Authorization": "Bearer abc123", "Accept": "application/json"}"#,
                format!(r#"{{"Authorization": "{REDACTED}", "Accept": "application/json"}}"#),
            ),
            (
                "`X-Appwrite-Key: abc123` returned 401",
                format!("`X-Appwrite-Key: {REDACTED}` returned 401"),
            ),
            (
                "headers: { authorization: 'Bearer abc123' }",
                format!("headers: {{ authorization: '{REDACTED}' }}"),
            ),
            (
                r#"-H 'Cookie: a="b"; c=d' -v"#,
                format!("-H 'Cookie: {REDACTED}' -v"),
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(redact(text), expected, "{text}");
        }
    }

    #[test]
    fn bearer_and_basic_credentials_are_redacted_anywhere_ignoring_case() {
        let cases = [
            (
                "retried with Bearer abc123def456 and got 200",
                format!("retried with Bearer {REDACTED} and got 200"),
            ),
            ("BEARER eyJhbGciOiJIUzI1NiJ9", format!("BEARER {REDACTED}")),
            (
                "Signed in with bearer abc123def456.",
                format!("Signed in with bearer {REDACTED}."),
            ),
            (
                "sent basic dXNlcjpwYXNzd29yZA== twice",
                format!("sent basic {REDACTED} twice"),
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(redact(text), expected, "{text}");
        }
    }

    #[test]
    fn words_after_bearer_and_basic_are_kept() {
        for text in [
            "Basic functionality works",
            "Ran a basic health-check against /v1/health",
            "The bearer token was rotated",
            "Basic Auth is enabled",
            "Bearer tokens-are-rotated nightly",
            "The API uses Bearer authentication.",
        ] {
            assert_eq!(redact(text), text);
        }
    }

    #[test]
    fn known_secrets_are_redacted_wherever_they_appear() {
        let redactor = Redactor::new([KNOWN_SECRET]);
        for text in [
            format!("bot token {KNOWN_SECRET} leaked"),
            format!("https://discord.com/api/webhooks/1/{KNOWN_SECRET}?wait=true"),
            format!(r#"{{"value":"{KNOWN_SECRET}"}}"#),
        ] {
            assert_eq!(
                redactor.redact(&text),
                text.replace(KNOWN_SECRET, REDACTED),
                "{text}"
            );
        }
    }

    #[test]
    fn longer_known_secrets_win_over_secrets_they_contain() {
        let redactor = Redactor::new(["deploy-key-0001", "prefix-deploy-key-0001"]);

        assert_eq!(
            redactor.redact("keys prefix-deploy-key-0001 and deploy-key-0001"),
            format!("keys {REDACTED} and {REDACTED}")
        );
    }

    #[test]
    fn known_values_shorter_than_eight_characters_are_ignored() {
        let redactor = Redactor::new(["short", "1234567", "  padded  "]);
        let text = "short 1234567 padded values stay readable";

        assert_eq!(redactor.redact(text), text);
    }

    #[test]
    fn known_values_are_matched_without_surrounding_whitespace() {
        let redactor = Redactor::new([format!("{KNOWN_SECRET}\n")]);

        assert_eq!(
            redactor.redact(&format!("token {KNOWN_SECRET}.")),
            format!("token {REDACTED}.")
        );
    }

    #[test]
    fn text_without_secrets_is_unchanged() {
        let report = "- #42 New route LIVE PASS: GET https://cloud.appwrite.io/v1/health/version returned 200\n\
                      - #43 Basic functionality of the bearer flow LIVE PASS\n\
                      - #44 CI path filters INFRA\n\
                      Project qa-1044 (6a8415b8002ea65eec9c), GIT_AUTHOR_NAME=Claudear\n\
                      DEPLOY_QA_VERDICT: ALL_VERIFIED";

        assert_eq!(Redactor::new([KNOWN_SECRET]).redact(report), report);
    }

    #[test]
    fn known_token_prefixes_are_still_redacted() {
        for token in [
            "ghp_abc123XYZ456",
            "gho_abc123XYZ456",
            "ghr_abc123XYZ456",
            "github_pat_abc123",
            "xoxb-123-456-abc",
            "lin_api_abc123",
            "sntrys_abc123",
            "sk-ant-api03-abc123",
        ] {
            assert_eq!(
                redact(&format!("found {token} here")),
                format!("found {REDACTED} here"),
                "{token}"
            );
        }
    }

    #[test]
    fn pem_blocks_are_redacted_whole() {
        let key = "-----BEGIN PRIVATE KEY-----\nfake-key-material-one\nfake-key-material-two\n-----END PRIVATE KEY-----";

        assert_eq!(
            redact(&format!("key:\n{key}\nnext line")),
            format!("key:\n{REDACTED}\nnext line")
        );
        assert_eq!(
            redact("cut -----BEGIN PRIVATE KEY-----\nfake-key-material-one"),
            format!("cut {REDACTED}")
        );
    }

    #[test]
    fn url_passwords_are_redacted() {
        assert_eq!(
            redact("connected to postgres://qa:s3cret-pass@db.example.com:5432/app"),
            format!("connected to postgres://qa:{REDACTED}@db.example.com:5432/app")
        );
        let plain = "https://cloud.appwrite.io:443/v1/health and mailto:qa@appwrite.io";
        assert_eq!(redact(plain), plain);
    }

    #[test]
    fn secret_named_assignments_are_redacted() {
        let cases = [
            (
                "GITHUB_TOKEN=q9w8e7r6 HOME=/root",
                format!("GITHUB_TOKEN={REDACTED} HOME=/root"),
            ),
            (
                "export DB_PASSWORD='p a s s'",
                format!("export DB_PASSWORD={REDACTED}"),
            ),
            ("_APP_DB_PASS=q9w8e7r6", format!("_APP_DB_PASS={REDACTED}")),
            (
                "GET /v1/users?project=qa&api_key=q9w8e7r6&limit=5",
                format!("GET /v1/users?project=qa&api_key={REDACTED}&limit=5"),
            ),
            (
                r#"{"name": "qa", "secret": "standard_q9w8e7r6", "apiKey": "q9w8e7r6"}"#,
                format!(r#"{{"name": "qa", "secret": "{REDACTED}", "apiKey": "{REDACTED}"}}"#),
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(redact(text), expected, "{text}");
        }
    }

    #[test]
    fn non_secret_assignments_are_kept() {
        for text in [
            "GIT_AUTHOR_NAME=Claudear GIT_AUTHOR_EMAIL=claudear@example.com",
            r#"{"key": "title", "twoWayKey": "posts", "$id": "6a8415b8002ea65eec9c"}"#,
            "GET /v1/databases?project=qa-1044&limit=25",
        ] {
            assert_eq!(redact(text), text);
        }
    }

    #[test]
    fn jwts_are_redacted() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.c2lnbmF0dXJl";

        assert_eq!(
            redact(&format!("session jwt {jwt} expired")),
            format!("session jwt {REDACTED} expired")
        );
    }

    #[test]
    fn secret_names_follow_credential_naming_conventions() {
        for name in [
            "GITHUB_TOKEN",
            "discord_bot_token",
            "AWS_SECRET_ACCESS_KEY",
            "DB_PASSWORD",
            "MYSQL_PASSWD",
            "OPENAI_API_KEY",
            "apiKey",
            "SSH_PRIVATE_KEY",
            "CLAUDEAR_ENCRYPTION_KEY",
            "_APP_DB_PASS",
            "NPM_AUTH",
            "Authorization",
            "X-Api-Key",
            "APPWRITE_JWT",
            "SESSION_COOKIE",
        ] {
            assert!(is_secret_name(name), "{name} should be secret");
        }
        for name in [
            "PATH",
            "HOME",
            "GIT_AUTHOR_NAME",
            "GIT_AUTHOR_EMAIL",
            "KEYBOARD",
            "key",
            "twoWayKey",
            "PASSTHROUGH",
        ] {
            assert!(!is_secret_name(name), "{name} should not be secret");
        }
    }

    #[test]
    fn variables_with_secret_names_are_redacted_by_value() {
        let redactor = Redactor::default().with_variables([
            ("APPWRITE_API_KEY", "q9w8e7r6t5y4"),
            ("HOME", "/home/claudear"),
        ]);

        assert_eq!(
            redactor.redact("key q9w8e7r6t5y4 in /home/claudear"),
            format!("key {REDACTED} in /home/claudear")
        );
    }

    #[test]
    fn redaction_is_idempotent() {
        let redactor = Redactor::new([KNOWN_SECRET]);
        let once = redactor.redact(&format!(
            "Authorization: Bearer abc123\ntoken {KNOWN_SECRET} ghp_abc123 postgres://u:p4ss@h/db"
        ));

        assert_eq!(redactor.redact(&once), once);
    }

    #[test]
    fn debug_output_hides_known_secrets() {
        let redactor = Redactor::new([KNOWN_SECRET]);

        assert!(!format!("{redactor:?}").contains(KNOWN_SECRET));
    }
}

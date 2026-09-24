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

/// Leading name segments that mark a credential, as in Appwrite's
/// `a_session_<project ID>` session cookies.
const SECRET_NAME_PREFIXES: &[&str] = &["A_SESSION"];

/// Name segments that contain a marker without naming a credential, as in
/// `GIT_AUTHOR_NAME`.
const NON_SECRET_NAME_SEGMENTS: &[&str] = &["AUTHOR", "AUTHORS"];

/// Labels that name a credential only when the value after them looks
/// generated, as `key` does in `API key: …` but not in `{"key": "title"}`.
const CREDENTIAL_LABELS: &[&str] = &["KEY", "KEYS"];

/// Shortest token taken as generated rather than written; shorter ones are
/// mostly words and identifiers.
const MINIMUM_GENERATED_LENGTH: usize = 20;

/// Appwrite API key types whose secret is hex, as in `standard_<hex>`.
const APPWRITE_HEX_KEY_TYPES: &[&str] = &["standard", "organization", "account"];

/// Appwrite API key types whose secret is a JWT, as in `ephemeral_<JWT>`;
/// `dynamic` is the older name for `ephemeral`.
const APPWRITE_JWT_KEY_TYPES: &[&str] = &["ephemeral", "dynamic"];

/// Shortest secret masked after a hex Appwrite key type. Real keys have 256
/// hex digits; a shorter minimum would mask identifiers like
/// `account_2faRecoveryCodes`.
const MINIMUM_APPWRITE_KEY_SECRET_LENGTH: usize = 32;

/// Characters found in tokens but not in words.
const TOKEN_SYMBOLS: &str = "_.~+/=";

/// Borders a markdown table cell: it ends an unquoted header value in a
/// cell, and stands in for the colon after a header's name or a label.
const TABLE_CELL_BORDER: char = '|';

/// Markdown emphasis that may wrap a header's name or value.
const EMPHASIS: char = '*';

/// Capture group holding the text to mask.
const SECRET_GROUP: &str = "secret";

/// Capture group holding a variable, field or label name.
const NAME_GROUP: &str = "name";

/// Capture group holding a command-line flag's name, without its `--`.
const FLAG_GROUP: &str = "flag";

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

/// A value after a label or command-line flag: `API key: value`,
/// `key=value`, `--key=value`, `--key value`, or a markdown table row's
/// `| key | value |`. The label and the value may be quoted or emphasised.
static LABELLED_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?:--(?<{FLAG_GROUP}>[A-Za-z][A-Za-z0-9_-]*)[ \t]+|\b(?<{NAME_GROUP}>[A-Za-z][A-Za-z0-9_-]*)["'`{EMPHASIS}]*[ \t]*[:={TABLE_CELL_BORDER}])[ \t"'`{EMPHASIS}]*(?<{SECRET_GROUP}>[A-Za-z0-9._~+/-]*[A-Za-z0-9_~+/-]=*)"#
    ))
    .expect("labelled value pattern is valid")
});

/// An Appwrite API key: its type and `_`, then an alphanumeric secret of at
/// least [`MINIMUM_APPWRITE_KEY_SECRET_LENGTH`] characters or, for the JWT
/// key types, a JWT.
static APPWRITE_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"\b(?<{SECRET_GROUP}>(?:{})_[A-Za-z0-9]{{{MINIMUM_APPWRITE_KEY_SECRET_LENGTH},}}|(?:{})_eyJ[A-Za-z0-9_.-]*[A-Za-z0-9_-])",
        APPWRITE_HEX_KEY_TYPES.join("|"),
        APPWRITE_JWT_KEY_TYPES.join("|")
    ))
    .expect("Appwrite key pattern is valid")
});

/// A JSON Web Token: base64url JSON header and payload, then the signature.
/// It may follow `_`, as the secret of a typed token like an Appwrite
/// `ephemeral_` key does.
static JWT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?:\b|_)(?<{SECRET_GROUP}>eyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]*)"
    ))
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
    /// assignments and JSON fields (Appwrite `a_session_*` cookies among
    /// them), Appwrite API keys, generated-looking values after a credential
    /// label or flag such as `API key:` or `--key`, JWTs, and tokens with a
    /// known prefix.
    pub fn redact(&self, text: &str) -> String {
        let text = self.redact_known(text);
        let text = PEM_BLOCK.replace_all(&text, NoExpand(REDACTED));
        let text = redact_credential_headers(&text);
        let text = redact_matches(&text, &BEARER_CREDENTIALS, is_credential);
        let text = redact_matches(&text, &BASIC_CREDENTIALS, is_credential);
        let text = redact_matches(&text, &URL_PASSWORD, |_, _| true);
        let text = redact_matches(&text, &VARIABLE_ASSIGNMENT, has_secret_name);
        let text = redact_matches(&text, &JSON_FIELD, has_secret_name);
        let text = redact_matches(&text, &APPWRITE_KEY, |_, key| looks_generated(key));
        let text = redact_matches(&text, &LABELLED_VALUE, is_labelled_credential);
        let text = redact_matches(&text, &JWT, |_, _| true);
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

/// Whether a labelled value is a credential: its label or flag names one,
/// and the value looks generated.
fn is_labelled_credential(value: &Captures<'_>, token: &str) -> bool {
    value
        .name(NAME_GROUP)
        .or_else(|| value.name(FLAG_GROUP))
        .is_some_and(|label| is_credential_label(label.as_str()))
        && looks_generated(token)
}

/// Whether `label` names a credential: by naming convention, as
/// [`is_secret_name`] decides, or as one of [`CREDENTIAL_LABELS`].
fn is_credential_label(label: &str) -> bool {
    is_secret_name(label)
        || CREDENTIAL_LABELS
            .iter()
            .any(|name| label.eq_ignore_ascii_case(name))
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

/// Whether `token` looks generated rather than written: at least
/// [`MINIMUM_GENERATED_LENGTH`] characters, mixing letters and digits, which
/// identifiers like `createEmailPasswordSession` do not.
fn looks_generated(token: &str) -> bool {
    token.chars().count() >= MINIMUM_GENERATED_LENGTH
        && token.contains(|character: char| character.is_ascii_digit())
        && token.contains(|character: char| character.is_ascii_alphabetic())
}

/// Whether a variable or field called `name` holds a credential by naming
/// convention: `GITHUB_TOKEN`, `DB_PASSWORD`, `apiKey`, `ENCRYPTION_KEY` and
/// `a_session_console` do; `GIT_AUTHOR_NAME` and `twoWayKey` do not.
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
        || SECRET_NAME_PREFIXES.iter().any(|prefix| {
            name.strip_prefix(prefix)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('_'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KNOWN_SECRET: &str = "configured-bot-token-0001";

    const SAMPLE_JWT: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjMifQ.c2lnbmF0dXJl";

    /// A token shaped like a generated credential, with no known prefix.
    const GENERATED_TOKEN: &str = "9fK2mQ7xL4pR8sT1vW3yZ6aB";

    /// A hex Appwrite API key from before keys carried their type.
    const LEGACY_APPWRITE_KEY: &str =
        "4f1c9a8b7e6d5c4b3a2f1e0d9c8b7a6f5e4d3c2b1a0f9e8d7c6b5a4f3e2d1c0b";

    /// An encoded Appwrite session, as its session cookie holds it.
    const SESSION: &str = "eyJpZCI6IjZhODQxNWI4MDAyZWE2NWVlYzljIiwic2VjcmV0IjoiNGYxYyJ9";

    fn redact(text: &str) -> String {
        Redactor::default().redact(text)
    }

    /// An Appwrite API key of every type: hex project, organization and
    /// account keys, and JWT ephemeral keys under their current and former
    /// names.
    fn appwrite_keys() -> Vec<String> {
        ["standard", "organization", "account"]
            .map(|kind| format!("{kind}_{LEGACY_APPWRITE_KEY}"))
            .into_iter()
            .chain(["ephemeral", "dynamic"].map(|kind| format!("{kind}_{SAMPLE_JWT}")))
            .collect()
    }

    /// `credential` labelled the ways a live-QA report labels a key: in
    /// prose, after a command-line flag and in a query string.
    fn labelled_as_key(credential: &str) -> [String; 6] {
        [
            format!("created API key: {credential} for qa-1044"),
            format!("key: {credential}"),
            format!("appwrite client --key {credential} --project-id qa-1044"),
            format!("appwrite client --key={credential}"),
            format!("GET /v1/health?key={credential}"),
            format!("GET /v1/users?project=qa-1044&key={credential}&limit=5"),
        ]
    }

    /// `credential` mentioned without a label: in backticks and in prose.
    fn unlabelled(credential: &str) -> [String; 2] {
        [
            format!("set `{credential}` on the QA function"),
            format!("rotated {credential} after the check"),
        ]
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
        assert_eq!(
            redact(&format!("session jwt {SAMPLE_JWT} expired")),
            format!("session jwt {REDACTED} expired")
        );
    }

    #[test]
    fn jwts_after_an_underscore_are_redacted() {
        assert_eq!(
            redact(&format!("minted refresh_{SAMPLE_JWT} for qa-1044")),
            format!("minted refresh_{REDACTED} for qa-1044")
        );
    }

    #[test]
    fn appwrite_api_keys_are_redacted_wherever_they_appear() {
        for key in appwrite_keys() {
            let texts = labelled_as_key(&key).into_iter().chain(unlabelled(&key));
            let redacted = labelled_as_key(REDACTED)
                .into_iter()
                .chain(unlabelled(REDACTED));
            for (text, expected) in texts.zip(redacted) {
                assert_eq!(redact(&text), expected, "{text}");
            }
        }
    }

    #[test]
    fn generated_values_after_a_credential_label_are_redacted() {
        for credential in [GENERATED_TOKEN, LEGACY_APPWRITE_KEY] {
            for (text, expected) in labelled_as_key(credential)
                .into_iter()
                .zip(labelled_as_key(REDACTED))
            {
                assert_eq!(redact(&text), expected, "{text}");
            }
        }
        let cases = [
            (
                format!("**API key:** `{GENERATED_TOKEN}`"),
                format!("**API key:** `{REDACTED}`"),
            ),
            (
                format!("key: {GENERATED_TOKEN}."),
                format!("key: {REDACTED}."),
            ),
            (
                format!("Access token: {GENERATED_TOKEN}"),
                format!("Access token: {REDACTED}"),
            ),
            (
                format!(r#"appwrite login --api-key "{GENERATED_TOKEN}""#),
                format!(r#"appwrite login --api-key "{REDACTED}""#),
            ),
            (
                format!("| API key | {GENERATED_TOKEN} |"),
                format!("| API key | {REDACTED} |"),
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(redact(&text), expected, "{text}");
        }
    }

    #[test]
    fn appwrite_session_cookies_are_redacted() {
        let cases = [
            (
                format!("curl -b 'a_session_6a8415b8002ea65eec9c={SESSION}' https://cloud.appwrite.io/v1/account"),
                format!("curl -b 'a_session_6a8415b8002ea65eec9c={REDACTED}' https://cloud.appwrite.io/v1/account"),
            ),
            (
                format!("got a_session_6a8415b8002ea65eec9c_legacy={SESSION}; Path=/; HttpOnly"),
                format!("got a_session_6a8415b8002ea65eec9c_legacy={REDACTED}; Path=/; HttpOnly"),
            ),
            (
                format!(r#"cookieFallback: {{"a_session_console": "{SESSION}"}}"#),
                format!(r#"cookieFallback: {{"a_session_console": "{REDACTED}"}}"#),
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(redact(&text), expected, "{text}");
        }
    }

    #[test]
    fn words_and_identifiers_that_resemble_credentials_are_kept() {
        for text in [
            "p95 standard_deviation stayed under 40ms",
            "dynamic_programming cache hit rate 98%",
            "standard_deviation_of_response_times_in_milliseconds: 12",
            "Attribute key: standard_deviation",
            "- #1234 key rotation LIVE PASS",
            "- #42 Auth: createEmailPasswordSession returns 201 LIVE PASS",
            "account_createEmailPasswordSessionWithMfaChallenge and account_2faRecoveryCodes",
            "key: title, Primary key: $id, API key: rotated",
            "| Key | Value |",
            "Request ID: 9fK2mQ7xL4pR8sT1vW3yZ6aB",
            "commit 4f1c9a8b7e6d5c4b3a2f1e0d9c8b7a6f5e4d3c2b",
            "Released 1.2.3-db to fra.cloud.appwrite.io and nyc.cloud.appwrite.io:443",
            "DEPLOY_QA_VERDICT: ALL_VERIFIED",
        ] {
            assert_eq!(redact(text), text);
        }
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
            "a_session",
            "a_session_console",
            "a_session_6a8415b8002ea65eec9c_legacy",
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
            "SESSION_ID",
            "a_sessions",
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
            "Authorization: Bearer abc123\ntoken {KNOWN_SECRET} ghp_abc123 postgres://u:p4ss@h/db\n\
             API key: {GENERATED_TOKEN} --key dynamic_{SAMPLE_JWT} a_session_console={SESSION}"
        ));

        assert_eq!(redactor.redact(&once), once);
    }

    #[test]
    fn debug_output_hides_known_secrets() {
        let redactor = Redactor::new([KNOWN_SECRET]);

        assert!(!format!("{redactor:?}").contains(KNOWN_SECRET));
    }
}

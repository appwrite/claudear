//! Credential redaction for untrusted text leaving Claudear.

use super::{redact_secrets, SecretValue, REDACTED};
use base64::alphabet;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::engine::DecodePaddingMode;
use base64::Engine;
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

/// Decodes the base64 of `Basic` credentials, padded or not, and even when
/// the text cuts it short.
const BASIC_CREDENTIALS_ENCODING: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

/// Separates the user from the password in decoded `Basic` credentials.
const USER_PASSWORD_SEPARATOR: char = ':';

/// Borders a markdown table cell: it ends an unquoted header value in a
/// cell, and stands in for the colon after a header's name or a label.
const TABLE_CELL_BORDER: char = '|';

/// Markdown emphasis that may wrap a header's name or value.
const EMPHASIS: char = '*';

/// Key of the field naming a cookie or header in the JSON objects browser
/// tools write, as in `{"name": "a_session_…", "value": "…"}`.
const NAME_KEY: &str = "name";

/// Key of the field holding the named cookie's or header's value.
const VALUE_KEY: &str = "value";

/// What may separate two fields of one JSON object: anything but a brace
/// outside a string, so both fields belong to the same object, never to
/// neighbouring or nested ones.
const SAME_OBJECT_GAP: &str = r#"(?:[^{}"]|"(?:[^"\\\r\n]|\\.)*")*?"#;

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

/// A JSON object's `"name"` field, then its `"value"` field, as browser tools
/// write a cookie or header.
static NAME_THEN_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        "{}{SAME_OBJECT_GAP}{}",
        json_string_field(NAME_KEY, NAME_GROUP),
        json_string_field(VALUE_KEY, SECRET_GROUP)
    ))
    .expect("name-then-value pattern is valid")
});

/// A JSON object's `"value"` field, then its `"name"` field.
static VALUE_THEN_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        "{}{SAME_OBJECT_GAP}{}",
        json_string_field(VALUE_KEY, SECRET_GROUP),
        json_string_field(NAME_KEY, NAME_GROUP)
    ))
    .expect("value-then-name pattern is valid")
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
    /// secrets, PEM blocks, credential header values, every value of eight or
    /// more characters after `Bearer`, `Basic` credentials, URL passwords, the
    /// values of secret-named `NAME=value` assignments and JSON fields
    /// (Appwrite `a_session_*` cookies among them), the `"value"` of each JSON
    /// object whose `"name"` is secret-named, as browser tools list cookies
    /// and headers, Appwrite API keys, generated-looking values after a
    /// credential label or flag such as `API key:` or `--key`, JWTs, and
    /// tokens with a known prefix.
    pub fn redact(&self, text: &str) -> String {
        let text = self.redact_known(text);
        let text = PEM_BLOCK.replace_all(&text, NoExpand(REDACTED));
        let text = redact_credential_headers(&text);
        let text = redact_matches(&text, &BEARER_CREDENTIALS, is_bearer_credential);
        let text = redact_matches(&text, &BASIC_CREDENTIALS, is_basic_credential);
        let text = redact_matches(&text, &URL_PASSWORD, |_, _| true);
        let text = redact_matches(&text, &VARIABLE_ASSIGNMENT, has_secret_name);
        let text = redact_matches(&text, &JSON_FIELD, has_secret_name);
        let text = redact_matches(&text, &NAME_THEN_VALUE, has_secret_name);
        let text = redact_matches(&text, &VALUE_THEN_NAME, has_secret_name);
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

/// Whether the value after `Bearer` is a credential: the scheme says it is,
/// whatever characters it uses, unless it is too short to be one, like the
/// `token` in "Bearer token".
fn is_bearer_credential(_: &Captures<'_>, token: &str) -> bool {
    token.chars().count() >= MINIMUM_SECRET_LENGTH
}

/// Whether the value after `Basic` is a credential rather than a word, as in
/// "basic functionality": shaped like a credential, or base64 of the
/// `user:password` pair every `Basic` credential encodes, whatever characters
/// that base64 happens to use.
fn is_basic_credential(_: &Captures<'_>, token: &str) -> bool {
    looks_like_credential(token) || encodes_user_password(token)
}

/// Whether `token` is base64 of a `user:password` pair: at least
/// [`MINIMUM_SECRET_LENGTH`] characters that decode to text holding a
/// [`USER_PASSWORD_SEPARATOR`] and no control characters.
fn encodes_user_password(token: &str) -> bool {
    token.chars().count() >= MINIMUM_SECRET_LENGTH
        && BASIC_CREDENTIALS_ENCODING
            .decode(token)
            .ok()
            .and_then(|decoded| String::from_utf8(decoded).ok())
            .is_some_and(|pair| {
                pair.contains(USER_PASSWORD_SEPARATOR) && !pair.contains(char::is_control)
            })
}

/// A pattern for a JSON field keyed `key` whose value is a non-empty string,
/// capturing that string's content as `group`.
fn json_string_field(key: &str, group: &str) -> String {
    format!(r#""{key}"\s*:\s*"(?<{group}>(?:[^"\\\r\n]|\\.)+)""#)
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

    /// A `Bearer` credential of lowercase letters only: no digit, symbol or
    /// capital marks it as generated.
    const LOWERCASE_BEARER_CREDENTIAL: &str = "abcdefghijklmnop";

    /// `nick:organic` as `Basic` credentials: base64 that happens to be
    /// lowercase letters only, so no digit, symbol or capital marks it as
    /// generated.
    const LOWERCASE_BASIC_CREDENTIALS: &str = "bmljazpvcmdhbmlj";

    fn redact(text: &str) -> String {
        Redactor::default().redact(text)
    }

    /// Every Appwrite API key type with a key of that type: hex project,
    /// organization and account keys, and JWT ephemeral keys under their
    /// current and former names.
    fn appwrite_keys() -> Vec<(&'static str, String)> {
        ["standard", "organization", "account"]
            .map(|kind| (kind, format!("{kind}_{LEGACY_APPWRITE_KEY}")))
            .into_iter()
            .chain(["ephemeral", "dynamic"].map(|kind| (kind, format!("{kind}_{SAMPLE_JWT}"))))
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
            assert_eq!(
                redact(header),
                format!("{name}: {REDACTED}"),
                "the {name} header's value must be masked"
            );
        }
    }

    #[test]
    fn unquoted_header_values_run_to_the_end_of_the_line_or_table_cell() {
        let cases = [
            (
                "a header line of an HTTP message",
                "> Authorization: Bearer abc123 (expired) → 401\r\n> Accept: */*",
                format!("> Authorization: {REDACTED}\r\n> Accept: */*"),
            ),
            (
                "a header whose value names another header",
                "Cookie: a_session=abc123; Authorization: Bearer abc123",
                format!("Cookie: {REDACTED}"),
            ),
            (
                "a header's name and value in separate table cells",
                "| X-Appwrite-Key | standard_abc123 | 401 |",
                format!("| X-Appwrite-Key | {REDACTED} | 401 |"),
            ),
            (
                "a whole header in one table cell",
                "| #42 | Authorization: Bearer abc123 | LIVE FAIL |",
                format!("| #42 | Authorization: {REDACTED} | LIVE FAIL |"),
            ),
        ];
        for (case, text, expected) in cases {
            assert_eq!(redact(text), expected, "{case}");
        }
    }

    #[test]
    fn emphasised_header_names_and_values_are_redacted() {
        let cases = [
            (
                "an emphasised header name",
                "**X-Appwrite-Key**: standard_abc123",
                format!("**X-Appwrite-Key**: {REDACTED}"),
            ),
            (
                "an emphasised header name and colon",
                "**Authorization:** Bearer abc123",
                format!("**Authorization:** {REDACTED}"),
            ),
            (
                "an emphasised header value",
                "Authorization: **Bearer abc123**",
                format!("Authorization: **{REDACTED}**"),
            ),
        ];
        for (case, text, expected) in cases {
            assert_eq!(redact(text), expected, "{case}");
        }
    }

    #[test]
    fn quoted_header_values_end_at_their_closing_quote() {
        let cases = [
            (
                "a double-quoted curl header before a single-quoted one",
                r#"curl -H "Authorization: Bearer abc123" -H 'X-Appwrite-Project: qa' https://cloud.appwrite.io/v1/account"#,
                format!(
                    r#"curl -H "Authorization: {REDACTED}" -H 'X-Appwrite-Project: qa' https://cloud.appwrite.io/v1/account"#
                ),
            ),
            (
                "two double-quoted curl headers",
                r#"curl -H "Authorization: Bearer abc123" -H "X-Appwrite-Key: key123" https://cloud.appwrite.io"#,
                format!(
                    r#"curl -H "Authorization: {REDACTED}" -H "X-Appwrite-Key: {REDACTED}" https://cloud.appwrite.io"#
                ),
            ),
            (
                "a JSON object of headers",
                r#"{"Authorization": "Bearer abc123", "Accept": "application/json"}"#,
                format!(r#"{{"Authorization": "{REDACTED}", "Accept": "application/json"}}"#),
            ),
            (
                "a header in a code span",
                "`X-Appwrite-Key: abc123` returned 401",
                format!("`X-Appwrite-Key: {REDACTED}` returned 401"),
            ),
            (
                "a JavaScript object of headers",
                "headers: { authorization: 'Bearer abc123' }",
                format!("headers: {{ authorization: '{REDACTED}' }}"),
            ),
            (
                "a single-quoted cookie header holding double quotes",
                r#"-H 'Cookie: a="b"; c=d' -v"#,
                format!("-H 'Cookie: {REDACTED}' -v"),
            ),
        ];
        for (case, text, expected) in cases {
            assert_eq!(redact(text), expected, "{case}");
        }
    }

    #[test]
    fn bearer_and_basic_credentials_are_redacted_anywhere_ignoring_case() {
        let cases = [
            (
                "a Bearer token in prose",
                "retried with Bearer abc123def456 and got 200",
                format!("retried with Bearer {REDACTED} and got 200"),
            ),
            (
                "an uppercase Bearer scheme",
                "BEARER eyJhbGciOiJIUzI1NiJ9",
                format!("BEARER {REDACTED}"),
            ),
            (
                "a lowercase Bearer scheme before a full stop",
                "Signed in with bearer abc123def456.",
                format!("Signed in with bearer {REDACTED}."),
            ),
            (
                "a lowercase Basic scheme in prose",
                "sent basic dXNlcjpwYXNzd29yZA== twice",
                format!("sent basic {REDACTED} twice"),
            ),
        ];
        for (case, text, expected) in cases {
            assert_eq!(redact(text), expected, "{case}");
        }
    }

    #[test]
    fn lowercase_bearer_credentials_are_redacted_anywhere() {
        let cases = [
            (
                "prose",
                format!("retried with Bearer {LOWERCASE_BEARER_CREDENTIAL} and got 200"),
                format!("retried with Bearer {REDACTED} and got 200"),
            ),
            (
                "prose ending in a full stop",
                format!("Signed in with bearer {LOWERCASE_BEARER_CREDENTIAL}."),
                format!("Signed in with bearer {REDACTED}."),
            ),
            (
                "a header Claudear does not know",
                format!(
                    r#"curl -H "X-Access-Token: Bearer {LOWERCASE_BEARER_CREDENTIAL}" https://cloud.appwrite.io/v1/account"#
                ),
                format!(
                    r#"curl -H "X-Access-Token: Bearer {REDACTED}" https://cloud.appwrite.io/v1/account"#
                ),
            ),
            (
                "curl's --oauth2-bearer flag",
                format!(
                    "curl --oauth2-bearer {LOWERCASE_BEARER_CREDENTIAL} https://cloud.appwrite.io/v1/account"
                ),
                format!("curl --oauth2-bearer {REDACTED} https://cloud.appwrite.io/v1/account"),
            ),
            (
                "a JSON header object",
                format!(
                    r#"{{"name": "authorization", "value": "Bearer {LOWERCASE_BEARER_CREDENTIAL}"}}"#
                ),
                format!(r#"{{"name": "authorization", "value": "{REDACTED}"}}"#),
            ),
        ];
        for (case, text, expected) in cases {
            assert_eq!(redact(&text), expected, "{case}");
        }
    }

    #[test]
    fn basic_credentials_are_redacted_whatever_characters_their_base64_uses() {
        let cases = [
            (
                "prose",
                format!("retried with basic {LOWERCASE_BASIC_CREDENTIALS} and got 200"),
                format!("retried with basic {REDACTED} and got 200"),
            ),
            (
                "a JSON header object",
                format!(
                    r#"{{"name": "authorization", "value": "Basic {LOWERCASE_BASIC_CREDENTIALS}"}}"#
                ),
                format!(r#"{{"name": "authorization", "value": "{REDACTED}"}}"#),
            ),
        ];
        for (case, text, expected) in cases {
            assert_eq!(redact(&text), expected, "{case}");
        }
    }

    #[test]
    fn words_after_bearer_and_basic_are_kept() {
        for text in [
            "Basic functionality works",
            "Ran a basic health-check against /v1/health",
            "Basic validation of every endpoint passed",
            "The bearer token was rotated",
            "Bearer tokens expire hourly",
            "Basic Auth is enabled",
        ] {
            assert_eq!(redact(text), text);
        }
    }

    #[test]
    fn known_secrets_are_redacted_wherever_they_appear() {
        let redactor = Redactor::new([KNOWN_SECRET]);
        for (case, text) in [
            ("prose", format!("bot token {KNOWN_SECRET} leaked")),
            (
                "a webhook URL",
                format!("https://discord.com/api/webhooks/1/{KNOWN_SECRET}?wait=true"),
            ),
            ("a JSON value", format!(r#"{{"value":"{KNOWN_SECRET}"}}"#)),
        ] {
            assert_eq!(
                redactor.redact(&text),
                text.replace(KNOWN_SECRET, REDACTED),
                "{case}"
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
        for (kind, value) in [
            ("GitHub personal access token", "ghp_abc123XYZ456"),
            ("GitHub OAuth token", "gho_abc123XYZ456"),
            ("GitHub refresh token", "ghr_abc123XYZ456"),
            ("GitHub fine-grained token", "github_pat_abc123"),
            ("Slack bot token", "xoxb-123-456-abc"),
            ("Linear API key", "lin_api_abc123"),
            ("Sentry system token", "sntrys_abc123"),
            ("Anthropic API key", "sk-ant-api03-abc123"),
        ] {
            assert_eq!(
                redact(&format!("found {value} here")),
                format!("found {REDACTED} here"),
                "the {kind} must be masked"
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
                "an environment variable",
                "GITHUB_TOKEN=q9w8e7r6 HOME=/root",
                format!("GITHUB_TOKEN={REDACTED} HOME=/root"),
            ),
            (
                "a quoted shell export",
                "export DB_PASSWORD='p a s s'",
                format!("export DB_PASSWORD={REDACTED}"),
            ),
            (
                "a name ending in _PASS",
                "_APP_DB_PASS=q9w8e7r6",
                format!("_APP_DB_PASS={REDACTED}"),
            ),
            (
                "a query string parameter",
                "GET /v1/users?project=qa&api_key=q9w8e7r6&limit=5",
                format!("GET /v1/users?project=qa&api_key={REDACTED}&limit=5"),
            ),
            (
                "JSON fields",
                r#"{"name": "qa", "secret": "standard_q9w8e7r6", "apiKey": "q9w8e7r6"}"#,
                format!(r#"{{"name": "qa", "secret": "{REDACTED}", "apiKey": "{REDACTED}"}}"#),
            ),
        ];
        for (case, text, expected) in cases {
            assert_eq!(redact(text), expected, "{case}");
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
        for (kind, key) in appwrite_keys() {
            let texts = labelled_as_key(&key).into_iter().chain(unlabelled(&key));
            let redacted = labelled_as_key(REDACTED)
                .into_iter()
                .chain(unlabelled(REDACTED));
            for (text, expected) in texts.zip(redacted) {
                assert_eq!(redact(&text), expected, "a {kind} key must be masked");
            }
        }
    }

    #[test]
    fn generated_values_after_a_credential_label_are_redacted() {
        for (kind, value) in [
            ("generated token", GENERATED_TOKEN),
            ("legacy Appwrite key", LEGACY_APPWRITE_KEY),
        ] {
            for (text, expected) in labelled_as_key(value)
                .into_iter()
                .zip(labelled_as_key(REDACTED))
            {
                assert_eq!(redact(&text), expected, "the {kind} must be masked");
            }
        }
        let cases = [
            (
                "an emphasised label before a code span",
                format!("**API key:** `{GENERATED_TOKEN}`"),
                format!("**API key:** `{REDACTED}`"),
            ),
            (
                "a value before a full stop",
                format!("key: {GENERATED_TOKEN}."),
                format!("key: {REDACTED}."),
            ),
            (
                "an access token label",
                format!("Access token: {GENERATED_TOKEN}"),
                format!("Access token: {REDACTED}"),
            ),
            (
                "a quoted flag value",
                format!(r#"appwrite login --api-key "{GENERATED_TOKEN}""#),
                format!(r#"appwrite login --api-key "{REDACTED}""#),
            ),
            (
                "a table row",
                format!("| API key | {GENERATED_TOKEN} |"),
                format!("| API key | {REDACTED} |"),
            ),
        ];
        for (case, text, expected) in cases {
            assert_eq!(redact(&text), expected, "{case}");
        }
    }

    #[test]
    fn appwrite_session_cookies_are_redacted() {
        let cases = [
            (
                "a cookie curl sends",
                format!("curl -b 'a_session_6a8415b8002ea65eec9c={SESSION}' https://cloud.appwrite.io/v1/account"),
                format!("curl -b 'a_session_6a8415b8002ea65eec9c={REDACTED}' https://cloud.appwrite.io/v1/account"),
            ),
            (
                "a legacy cookie a response sets",
                format!("got a_session_6a8415b8002ea65eec9c_legacy={SESSION}; Path=/; HttpOnly"),
                format!("got a_session_6a8415b8002ea65eec9c_legacy={REDACTED}; Path=/; HttpOnly"),
            ),
            (
                "the cookie fallback's JSON",
                format!(r#"cookieFallback: {{"a_session_console": "{SESSION}"}}"#),
                format!(r#"cookieFallback: {{"a_session_console": "{REDACTED}"}}"#),
            ),
        ];
        for (case, text, expected) in cases {
            assert_eq!(redact(&text), expected, "{case}");
        }
    }

    #[test]
    fn values_of_json_cookies_with_secret_names_are_redacted_in_either_order() {
        let cases = [
            (
                "a cookie as browser tools list it",
                format!(
                    r#"{{"name": "a_session_6a8415b8002ea65eec9c", "value": "{SESSION}", "domain": ".cloud.appwrite.io", "path": "/", "httpOnly": true, "sameSite": "Strict"}}"#
                ),
            ),
            (
                "a compact legacy cookie",
                format!(
                    r#"{{"name":"a_session_6a8415b8002ea65eec9c_legacy","value":"{SESSION}"}}"#
                ),
            ),
            (
                "a cookie with its value first",
                format!(r#"{{"value": "{SESSION}", "path": "/", "name": "a_session_console"}}"#),
            ),
            (
                "a compact cookie with its value first",
                format!(r#"{{"value":"{SESSION}","name":"a_session_console"}}"#),
            ),
            (
                "a cookie spaced around its colons",
                format!("{{ \"name\" : \"a_session_console\" , \"value\"\t:\t\"{SESSION}\" }}"),
            ),
            (
                "a pretty-printed cookie list",
                format!(
                    "[\n  {{\n    \"name\": \"theme\",\n    \"value\": \"dark\"\n  }},\n  {{\n    \"name\": \"a_session_console\",\n    \"domain\": \".cloud.appwrite.io\",\n    \"value\": \"{SESSION}\"\n  }}\n]"
                ),
            ),
        ];
        for (case, text) in cases {
            assert_eq!(redact(&text), text.replace(SESSION, REDACTED), "{case}");
        }
    }

    #[test]
    fn values_of_json_objects_without_a_secret_name_are_kept() {
        let cases = [
            (
                "a cookie that is no credential",
                r#"{"name": "theme", "value": "dark"}"#,
            ),
            (
                "that cookie with its value first",
                r#"{"value":"dark","name":"theme"}"#,
            ),
            (
                "a secret name in the object before",
                r#"[{"name": "a_session_console", "path": "/"}, {"path": "/", "value": "dark"}]"#,
            ),
            (
                "a secret name in the object after",
                r#"[{"value": "dark", "path": "/"}, {"path": "/", "name": "a_session_console"}]"#,
            ),
            (
                "a secret name in a nested object",
                r#"{"value": "dark", "partitionKey": {"name": "a_session_console"}}"#,
            ),
        ];
        for (case, text) in cases {
            assert_eq!(redact(text), text, "{case}");
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
             API key: {GENERATED_TOKEN} --key dynamic_{SAMPLE_JWT} a_session_console={SESSION}\n\
             {{\"value\": \"{SESSION}\", \"name\": \"a_session_console\"}}"
        ));

        assert_eq!(redactor.redact(&once), once);
    }

    #[test]
    fn debug_output_hides_known_secrets() {
        let redactor = Redactor::new([KNOWN_SECRET]);

        assert!(!format!("{redactor:?}").contains(KNOWN_SECRET));
    }
}

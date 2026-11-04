use super::field::IdentityField;
use super::violation::IdentityViolation;

/// Reserved for the `console-<user_id>` alias every console person keeps.
const RESERVED_SLUG_PREFIX: &str = "console-";
const MAXIMUM_SLUG_LENGTH: usize = 63;
const MAXIMUM_TEXT_LENGTH: usize = 255;
const MAXIMUM_GITHUB_LENGTH: usize = 39;
const MAXIMUM_GITLAB_LENGTH: usize = 255;
const MAXIMUM_EMAIL_LENGTH: usize = 254;
const DISCORD_ID_LENGTHS: std::ops::RangeInclusive<usize> = 17..=20;
const SLACK_SUFFIX_LENGTHS: std::ops::RangeInclusive<usize> = 8..=20;
const E164_DIGIT_LENGTHS: std::ops::RangeInclusive<usize> = 7..=15;
const TELEGRAM_DIGIT_LENGTHS: std::ops::RangeInclusive<usize> = 1..=20;
const PUSHOVER_KEY_LENGTH: usize = 30;
const INVALID_SLUG_MESSAGE: &str =
    "slug must be 1-63 characters of a-z, 0-9, '.', '_' or '-', starting with a letter or digit";

/// Validate an admin-set person key: `[a-z0-9]` followed by up to 62 of `[a-z0-9._-]`.
pub fn validate_slug(slug: &str) -> Result<(), IdentityViolation> {
    if slug.starts_with(RESERVED_SLUG_PREFIX) {
        return Err(IdentityViolation::new(
            IdentityField::Slug,
            "slug must not start with 'console-', which is reserved",
        ));
    }
    if is_slug(slug) {
        Ok(())
    } else {
        Err(IdentityViolation::new(
            IdentityField::Slug,
            INVALID_SLUG_MESSAGE,
        ))
    }
}

/// Validate one value of `field`; list fields are checked one entry at a time.
pub(crate) fn validate_value(field: IdentityField, value: &str) -> Result<(), IdentityViolation> {
    let valid = match field {
        IdentityField::Slug => return validate_slug(value),
        IdentityField::LinearNames
        | IdentityField::SentryUsernames
        | IdentityField::JiraUsernames => is_text(value),
        IdentityField::GithubUsernames => is_github_login(value),
        IdentityField::GitlabUsernames => is_gitlab_username(value),
        IdentityField::DiscordId => is_discord_id(value),
        IdentityField::SlackId => is_slack_id(value),
        IdentityField::Email => is_email(value),
        IdentityField::PushUserKey => is_pushover_key(value),
        IdentityField::SmsNumber | IdentityField::WhatsappNumber => is_e164(value),
        IdentityField::TelegramChatId => is_telegram_chat_id(value),
    };
    if valid {
        Ok(())
    } else {
        Err(IdentityViolation::new(field, message(field, value)))
    }
}

fn message(field: IdentityField, value: &str) -> String {
    match field {
        IdentityField::GithubUsernames => format!("'{value}' is not a valid GitHub login"),
        IdentityField::GitlabUsernames => format!("'{value}' is not a valid GitLab username"),
        IdentityField::LinearNames
        | IdentityField::SentryUsernames
        | IdentityField::JiraUsernames => {
            format!(
                "{} entries must be 1-255 characters with no control characters",
                field.as_str()
            )
        }
        IdentityField::DiscordId => "Discord ID must be a 17-20 digit Discord user ID".to_string(),
        IdentityField::SlackId => "Slack ID must look like U0123456789".to_string(),
        IdentityField::Email => "email must be a valid email address".to_string(),
        IdentityField::PushUserKey => {
            "push_user_key must be a 30-character Pushover user key".to_string()
        }
        IdentityField::SmsNumber | IdentityField::WhatsappNumber => {
            format!("{} must be E.164, e.g. +14155550123", field.as_str())
        }
        IdentityField::TelegramChatId => "telegram_chat_id must be a numeric chat ID".to_string(),
        IdentityField::Slug => INVALID_SLUG_MESSAGE.to_string(),
    }
}

fn is_digits(value: &str, lengths: std::ops::RangeInclusive<usize>) -> bool {
    lengths.contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_slug(value: &str) -> bool {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    value.len() <= MAXIMUM_SLUG_LENGTH
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn is_text(value: &str) -> bool {
    let length = value.chars().count();
    (1..=MAXIMUM_TEXT_LENGTH).contains(&length) && !value.chars().any(char::is_control)
}

fn is_github_login(value: &str) -> bool {
    (1..=MAXIMUM_GITHUB_LENGTH).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && !value.starts_with('-')
        && !value.ends_with('-')
        && !value.contains("--")
}

fn is_gitlab_username(value: &str) -> bool {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    value.len() <= MAXIMUM_GITLAB_LENGTH
        && first.is_ascii_alphanumeric()
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

fn is_discord_id(value: &str) -> bool {
    is_digits(value, DISCORD_ID_LENGTHS)
}

fn is_slack_id(value: &str) -> bool {
    let Some(suffix) = value.strip_prefix(['U', 'W']) else {
        return false;
    };
    SLACK_SUFFIX_LENGTHS.contains(&suffix.len())
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

fn is_email(value: &str) -> bool {
    if value.len() > MAXIMUM_EMAIL_LENGTH || value.chars().any(char::is_whitespace) {
        return false;
    }
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
}

fn is_e164(value: &str) -> bool {
    let Some(digits) = value.strip_prefix('+') else {
        return false;
    };
    is_digits(digits, E164_DIGIT_LENGTHS) && !digits.starts_with('0')
}

fn is_telegram_chat_id(value: &str) -> bool {
    let digits = value.strip_prefix('-').unwrap_or(value);
    is_digits(digits, TELEGRAM_DIGIT_LENGTHS)
}

fn is_pushover_key(value: &str) -> bool {
    value.len() == PUSHOVER_KEY_LENGTH && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_valid(field: IdentityField, values: &[&str]) {
        for value in values {
            assert!(
                validate_value(field, value).is_ok(),
                "{value:?} must be a valid {field}"
            );
        }
    }

    fn assert_invalid(field: IdentityField, values: &[&str]) {
        for value in values {
            let violation = validate_value(field, value)
                .expect_err(&format!("{value:?} must be an invalid {field}"));
            assert_eq!(violation.field, field);
            assert!(!violation.message.is_empty());
        }
    }

    #[test]
    fn discord_id_is_17_to_20_digits() {
        assert_valid(
            IdentityField::DiscordId,
            &[
                "12345678901234567",
                "452316113016193024",
                "12345678901234567890",
            ],
        );
        assert_invalid(
            IdentityField::DiscordId,
            &[
                "",
                "1234567890123456",
                "123456789012345678901",
                "45231611301619302a",
                " 452316113016193024",
                "<@452316113016193024>",
            ],
        );
    }

    #[test]
    fn github_login_rules() {
        let longest = "a".repeat(39);
        assert_valid(
            IdentityField::GithubUsernames,
            &[
                "a",
                "jakebarnby",
                "Jake-Barnby",
                "a-b-c",
                "user123",
                &longest,
            ],
        );
        let too_long = "a".repeat(40);
        assert_invalid(
            IdentityField::GithubUsernames,
            &[
                "", "a--b", "-a", "a-", "a_b", "a.b", "@jake", "jäke", &too_long,
            ],
        );
    }

    #[test]
    fn github_violation_names_the_login() {
        let violation =
            validate_value(IdentityField::GithubUsernames, "a--b").expect_err("invalid login");
        assert_eq!(violation.message, "'a--b' is not a valid GitHub login");
    }

    #[test]
    fn gitlab_username_rules() {
        let longest = "a".repeat(255);
        assert_valid(
            IdentityField::GitlabUsernames,
            &[
                "a",
                "jake.barnby",
                "jake_barnby",
                "jake-barnby",
                "9lives",
                &longest,
            ],
        );
        let too_long = "a".repeat(256);
        assert_invalid(
            IdentityField::GitlabUsernames,
            &[
                "",
                "_jake",
                ".jake",
                "-jake",
                "jake barnby",
                "jake@x",
                &too_long,
            ],
        );
    }

    #[test]
    fn slack_id_rules() {
        assert_valid(
            IdentityField::SlackId,
            &["U0123456789", "W01234567", "U01234567890123456789"],
        );
        assert_invalid(
            IdentityField::SlackId,
            &[
                "",
                "U",
                "U0123456",
                "U012345678901234567890",
                "u0123456789",
                "U0123abc89",
                "C0123456789",
                "B0123456789",
            ],
        );
    }

    #[test]
    fn email_rules() {
        let local = "a".repeat(240);
        let longest = format!("{local}@example.com");
        assert_eq!(longest.len(), 252);
        assert_valid(
            IdentityField::Email,
            &[
                "jake@appwrite.io",
                "a@b.c",
                "first.last+tag@sub.example.co.uk",
                &longest,
            ],
        );
        let too_long = format!("{}@example.com", "a".repeat(243));
        assert_eq!(too_long.len(), 255);
        assert_invalid(
            IdentityField::Email,
            &[
                "",
                "jake",
                "@appwrite.io",
                "jake@",
                "jake@appwrite",
                "jake@.appwrite.io",
                "jake@appwrite.io.",
                "jake@@appwrite.io",
                "ja@ke@appwrite.io",
                "jake @appwrite.io",
                "jake@appwrite.io\n",
                &too_long,
            ],
        );
    }

    #[test]
    fn e164_rules_for_sms_and_whatsapp() {
        for field in [IdentityField::SmsNumber, IdentityField::WhatsappNumber] {
            assert_valid(
                field,
                &[
                    "+1234567890",
                    "+14155550123",
                    "+1234567",
                    "+123456789012345",
                ],
            );
            assert_invalid(
                field,
                &[
                    "",
                    "+",
                    "1234567",
                    "14155550123",
                    "+0123456789",
                    "+123456",
                    "+1234567890123456",
                    "+1 415 555 0123",
                    "+1-415-555-0123",
                ],
            );
        }
    }

    #[test]
    fn e164_message_names_the_field() {
        let violation = validate_value(IdentityField::WhatsappNumber, "123").expect_err("invalid");
        assert_eq!(
            violation.message,
            "whatsapp_number must be E.164, e.g. +14155550123"
        );
    }

    #[test]
    fn telegram_chat_id_rules() {
        assert_valid(
            IdentityField::TelegramChatId,
            &["1", "123456789", "-1001234567890", "-12345678901234567890"],
        );
        assert_invalid(
            IdentityField::TelegramChatId,
            &[
                "",
                "-",
                "--1",
                "12a",
                "@channel",
                "123456789012345678901",
                "+123",
            ],
        );
    }

    #[test]
    fn pushover_key_is_30_alphanumerics() {
        assert_valid(
            IdentityField::PushUserKey,
            &[
                "uQiRzpo4DXghDmr9QzzfQu27cmVRsG",
                "abcdefghijklmnopqrstuvwxyz0123",
            ],
        );
        assert_invalid(
            IdentityField::PushUserKey,
            &[
                "",
                "uQiRzpo4DXghDmr9QzzfQu27cmVRs",
                "uQiRzpo4DXghDmr9QzzfQu27cmVRsGx",
                "uQiRzpo4DXghDmr9QzzfQu27cmVRs-",
            ],
        );
    }

    #[test]
    fn free_text_handles_rules() {
        let longest = "é".repeat(255);
        let too_long = "a".repeat(256);
        for field in [
            IdentityField::LinearNames,
            IdentityField::SentryUsernames,
            IdentityField::JiraUsernames,
        ] {
            assert_valid(field, &["Jake Barnby", "jake@appwrite.io", "a", &longest]);
            assert_invalid(
                field,
                &["", "Jake\nBarnby", "tab\there", "nul\0", &too_long],
            );
        }
    }

    #[test]
    fn slug_rules() {
        let longest = format!("a{}", "b".repeat(62));
        assert!(validate_slug(&longest).is_ok());
        for slug in [
            "jake",
            "0",
            "jake.barnby",
            "jake_barnby",
            "jake-barnby",
            "j4ke",
        ] {
            assert!(validate_slug(slug).is_ok(), "{slug:?} must be a valid slug");
        }
        let too_long = format!("a{}", "b".repeat(63));
        for slug in [
            "",
            "Jake",
            "jAke",
            "-jake",
            ".jake",
            "_jake",
            "jake barnby",
            "jake/barnby",
            "jäke",
            &too_long,
        ] {
            let violation = validate_slug(slug).expect_err(&format!("{slug:?} must be invalid"));
            assert_eq!(violation.field, IdentityField::Slug);
        }
    }

    #[test]
    fn slug_rejects_reserved_console_prefix() {
        for slug in ["console-1", "console-42", "console-"] {
            let violation = validate_slug(slug).expect_err("reserved prefix");
            assert_eq!(violation.field, IdentityField::Slug);
            assert!(violation.message.contains("reserved"));
        }
        assert!(validate_slug("console").is_ok());
        assert!(validate_slug("consoles").is_ok());
    }

    #[test]
    fn validate_value_delegates_slug() {
        assert!(validate_value(IdentityField::Slug, "jake").is_ok());
        assert!(validate_value(IdentityField::Slug, "console-1").is_err());
    }

    #[test]
    fn sensitive_violation_messages_never_echo_the_value() {
        let cases = [
            (IdentityField::Email, "secret-address"),
            (IdentityField::PushUserKey, "secretkey"),
            (IdentityField::SmsNumber, "5550123secret"),
            (IdentityField::WhatsappNumber, "5550123secret"),
            (IdentityField::TelegramChatId, "secretchat"),
        ];
        for (field, value) in cases {
            assert!(field.is_sensitive());
            let violation = validate_value(field, value).expect_err("invalid value");
            assert!(
                !violation.message.contains(value),
                "{field} message must not contain the value"
            );
        }
    }
}

/// The stored and compared form of a console login email: trimmed and lowercased, so
/// `Amy@Example.com` and `amy@example.com` are one account.
pub fn normalize_login_email(email: &str) -> String {
    email.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_and_lowercases() {
        for (raw, normalized) in [
            ("amy@example.com", "amy@example.com"),
            ("Amy@Example.COM", "amy@example.com"),
            ("  amy@example.com\t", "amy@example.com"),
            (" ÉLODIE@Example.com ", "élodie@example.com"),
            ("", ""),
            ("   ", ""),
        ] {
            assert_eq!(normalize_login_email(raw), normalized, "{raw:?}");
        }
    }

    #[test]
    fn is_idempotent() {
        let once = normalize_login_email(" Mixed.Case@Example.com ");
        assert_eq!(normalize_login_email(&once), once);
    }
}

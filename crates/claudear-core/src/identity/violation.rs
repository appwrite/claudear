use super::field::IdentityField;
use serde::Serialize;

/// A rejected identity value. The message never repeats a sensitive value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdentityViolation {
    pub field: IdentityField,
    pub message: String,
}

impl IdentityViolation {
    pub fn new(field: IdentityField, message: impl Into<String>) -> Self {
        Self {
            field,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_field_by_name() {
        let violation = IdentityViolation::new(IdentityField::DiscordId, "bad");
        let json = serde_json::to_value(&violation).expect("serialize violation");
        assert_eq!(
            json,
            serde_json::json!({"field": "discord_id", "message": "bad"})
        );
    }
}

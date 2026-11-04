use crate::config::UserConfig;

/// A delivery channel whose per-person address a notifier reference can resolve to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotificationChannel {
    Discord,
    Slack,
    Email,
    Push,
    Sms,
    Whatsapp,
    Telegram,
}

impl NotificationChannel {
    /// The person's address on this channel, if they have one.
    pub fn address(self, user: &UserConfig) -> Option<&str> {
        let address = match self {
            Self::Discord => &user.discord_id,
            Self::Slack => &user.slack_id,
            Self::Email => &user.email,
            Self::Push => &user.push_user_key,
            Self::Sms => &user.sms_number,
            Self::Whatsapp => &user.whatsapp_number,
            Self::Telegram => &user.telegram_chat_id,
        };
        address.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_reads_the_matching_field() {
        let user = UserConfig {
            discord_id: Some("discord".to_string()),
            slack_id: Some("slack".to_string()),
            email: Some("email".to_string()),
            push_user_key: Some("push".to_string()),
            sms_number: Some("sms".to_string()),
            whatsapp_number: Some("whatsapp".to_string()),
            telegram_chat_id: Some("telegram".to_string()),
            ..UserConfig::default()
        };
        for (channel, expected) in [
            (NotificationChannel::Discord, "discord"),
            (NotificationChannel::Slack, "slack"),
            (NotificationChannel::Email, "email"),
            (NotificationChannel::Push, "push"),
            (NotificationChannel::Sms, "sms"),
            (NotificationChannel::Whatsapp, "whatsapp"),
            (NotificationChannel::Telegram, "telegram"),
        ] {
            assert_eq!(channel.address(&user), Some(expected));
        }
        assert_eq!(
            NotificationChannel::Discord.address(&UserConfig::default()),
            None
        );
    }
}

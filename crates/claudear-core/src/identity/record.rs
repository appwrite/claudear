use super::identities::Identities;

/// A console user's saved identities joined with their login details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleIdentity {
    pub user_id: i64,
    pub login_email: String,
    pub name: String,
    pub slug: Option<String>,
    pub identities: Identities,
    pub updated_at: String,
}

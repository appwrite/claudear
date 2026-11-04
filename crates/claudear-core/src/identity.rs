//! Per-person identity handles (source logins, chat ids, notification targets) and their validation.

mod email;
mod field;
mod identities;
mod origin;
mod record;
mod role;
mod status;
mod validation;
mod violation;

pub use email::normalize_login_email;
pub use field::IdentityField;
pub use identities::Identities;
pub use origin::IdentityOrigin;
pub use record::ConsoleIdentity;
pub use role::Role;
pub use status::UserStatus;
pub use validation::validate_slug;
pub use violation::IdentityViolation;

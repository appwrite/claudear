//! Per-person identity handles (source logins, chat ids, notification targets) and their validation.

mod field;
mod identities;
mod origin;
mod record;
mod status;
mod validation;
mod violation;

pub use field::IdentityField;
pub use identities::Identities;
pub use origin::IdentityOrigin;
pub use record::{ConsoleIdentity, IdentityRecord};
pub use status::UserStatus;
pub use validation::validate_slug;
pub use violation::IdentityViolation;

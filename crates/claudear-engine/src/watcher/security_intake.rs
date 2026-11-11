/// Where a poll of the HelpScout source hands its security conversations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SecurityIntake {
    /// The poll routes them and advances the security inbox itself.
    Inline,
    /// A dedicated loop owns them, so the poll leaves them out.
    Separate,
}

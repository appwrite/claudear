use super::{Outcome, Reason};

/// How a shutdown went, for choosing the exit code.
#[derive(Debug)]
#[must_use]
pub struct Summary {
    /// What started the shutdown.
    pub reason: Reason,
    /// How the drain ended.
    pub outcome: Outcome,
    /// Why the service whose end started the shutdown ended: its error, or, when it returned
    /// `Ok`, that it stopped unexpectedly.
    pub error: Option<anyhow::Error>,
}

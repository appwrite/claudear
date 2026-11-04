#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Progress {
    Pending { rows: usize },
    Complete { rows: usize },
    Current,
    Skipped,
    Deferred,
    Unavailable,
}

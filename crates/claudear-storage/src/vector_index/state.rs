#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    Unbuilt,
    Building {
        dimension: usize,
        capacity: usize,
        cursor: i64,
        generation: u64,
    },
    Built {
        dimension: usize,
        capacity: usize,
        generation: u64,
    },
    Stale {
        dimension: usize,
    },
    Unavailable,
}

impl State {
    pub(crate) fn needs_rebuild(&self) -> bool {
        matches!(
            self,
            Self::Unbuilt | Self::Building { .. } | Self::Stale { .. }
        )
    }
}

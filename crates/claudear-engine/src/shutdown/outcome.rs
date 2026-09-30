/// How the drain ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Every in-flight run finished.
    Drained,
    /// Runs were still in flight when the drain gave up or
    /// [`DRAIN_TIMEOUT`](super::DRAIN_TIMEOUT) passed.
    TimedOut,
    /// Another signal cut the drain short.
    Forced,
}

impl Outcome {
    /// Stable snake_case identifier for activity metadata.
    pub fn label(self) -> &'static str {
        match self {
            Self::Drained => "drained",
            Self::TimedOut => "timed_out",
            Self::Forced => "forced",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_stable_snake_case() {
        assert_eq!(Outcome::Drained.label(), "drained");
        assert_eq!(Outcome::TimedOut.label(), "timed_out");
        assert_eq!(Outcome::Forced.label(), "forced");
    }
}

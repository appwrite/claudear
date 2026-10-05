use std::fmt;

/// Why the daemon started shutting down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// `claudear stop` asked the daemon to stop over IPC.
    Requested,
    /// SIGINT from Ctrl+C, or SIGHUP from a closed terminal.
    Interrupted,
    /// SIGTERM, typically from a service manager or `docker stop`.
    Terminated,
    /// The named service returned before anything asked the daemon to stop.
    ServiceEnded(&'static str),
}

impl Reason {
    /// Stable snake_case identifier for activity metadata.
    pub fn label(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Interrupted => "interrupted",
            Self::Terminated => "terminated",
            Self::ServiceEnded(_) => "service_ended",
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Requested => formatter.write_str("stop requested"),
            Self::Interrupted => formatter.write_str("interrupted"),
            Self::Terminated => formatter.write_str("terminated"),
            Self::ServiceEnded(name) => write!(formatter, "{name} service ended"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_stable_snake_case() {
        assert_eq!(Reason::Requested.label(), "requested");
        assert_eq!(Reason::Interrupted.label(), "interrupted");
        assert_eq!(Reason::Terminated.label(), "terminated");
        assert_eq!(Reason::ServiceEnded("ipc").label(), "service_ended");
    }

    #[test]
    fn display_describes_the_reason() {
        assert_eq!(Reason::Requested.to_string(), "stop requested");
        assert_eq!(Reason::Interrupted.to_string(), "interrupted");
        assert_eq!(Reason::Terminated.to_string(), "terminated");
        assert_eq!(Reason::ServiceEnded("ipc").to_string(), "ipc service ended");
    }
}

use std::fmt;

/// Why a rename cannot replace a destination that is already there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refusal {
    /// The destination is a mount point, such as a single file bind-mounted
    /// into a container. Renaming over it fails with EBUSY or EXDEV.
    MountPoint,
    /// Another user owns the destination in a sticky directory this user does
    /// not own either. Renaming over it fails with EPERM.
    ForeignFileInStickyDirectory,
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MountPoint => {
                "it is a mount point, such as a single file bind-mounted into a container, so a rename cannot replace it; mount its directory instead"
            }
            Self::ForeignFileInStickyDirectory => {
                "another user owns it in a sticky directory, so a rename cannot replace it; run as its owner or move it to a directory this user owns"
            }
        })
    }
}

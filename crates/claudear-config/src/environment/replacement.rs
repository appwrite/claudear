use std::fs;
use std::fs::File;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::mount_table::MountTable;
use super::refusal::Refusal;

const STICKY: u32 = 0o1000;
const SUPERUSER: u32 = 0;

/// What the kernel weighs when a rename replaces a destination that is
/// already there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Replacement {
    destination_device: u64,
    directory_device: u64,
    mounted: bool,
    sticky: bool,
    destination_owner: u32,
    directory_owner: u32,
    user: u32,
}

impl Replacement {
    /// The replacement of `destination` in `directory`, or `None` when nothing
    /// is there to replace. `created` is a file this process just created in
    /// `directory`: its owner is the user the kernel checks the rename as.
    pub(super) fn inspect(
        destination: &Path,
        directory: &Path,
        created: &File,
    ) -> io::Result<Option<Self>> {
        let target = match fs::symlink_metadata(destination) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let container = fs::metadata(directory)?;
        let mounted = match destination.file_name() {
            Some(name) => MountTable::read().contains(&fs::canonicalize(directory)?.join(name)),
            None => false,
        };

        Ok(Some(Self {
            destination_device: target.dev(),
            directory_device: container.dev(),
            mounted,
            sticky: container.mode() & STICKY != 0,
            destination_owner: target.uid(),
            directory_owner: container.uid(),
            user: created.metadata()?.uid(),
        }))
    }

    pub(super) fn refusal(&self) -> Option<Refusal> {
        if self.mounted || self.destination_device != self.directory_device {
            return Some(Refusal::MountPoint);
        }

        let privileged = [SUPERUSER, self.destination_owner, self.directory_owner];
        if self.sticky && !privileged.contains(&self.user) {
            return Some(Refusal::ForeignFileInStickyDirectory);
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    use tempfile::TempDir;

    const DEVICE: u64 = 66;
    const OTHER_DEVICE: u64 = 67;
    const USER: u32 = 501;
    const OTHER_USER: u32 = 502;

    fn ordinary() -> Replacement {
        Replacement {
            destination_device: DEVICE,
            directory_device: DEVICE,
            mounted: false,
            sticky: false,
            destination_owner: USER,
            directory_owner: USER,
            user: USER,
        }
    }

    fn foreign_in_sticky_directory() -> Replacement {
        Replacement {
            sticky: true,
            destination_owner: OTHER_USER,
            directory_owner: OTHER_USER,
            ..ordinary()
        }
    }

    #[test]
    fn test_an_own_file_on_its_directory_device_is_replaceable() {
        assert_eq!(ordinary().refusal(), None);
    }

    #[test]
    fn test_a_file_on_another_device_than_its_directory_is_a_mount_point() {
        let bind_mounted = Replacement {
            destination_device: OTHER_DEVICE,
            ..ordinary()
        };

        assert_eq!(bind_mounted.refusal(), Some(Refusal::MountPoint));
    }

    #[test]
    fn test_a_mount_point_on_the_same_device_as_its_directory_is_refused() {
        let bind_mounted = Replacement {
            mounted: true,
            ..ordinary()
        };

        assert_eq!(bind_mounted.refusal(), Some(Refusal::MountPoint));
    }

    #[test]
    fn test_another_users_file_in_another_users_sticky_directory_is_refused() {
        assert_eq!(
            foreign_in_sticky_directory().refusal(),
            Some(Refusal::ForeignFileInStickyDirectory)
        );
    }

    #[test]
    fn test_a_sticky_directory_lets_the_file_owner_the_directory_owner_and_root_replace() {
        let file_owner = Replacement {
            destination_owner: USER,
            ..foreign_in_sticky_directory()
        };
        let directory_owner = Replacement {
            directory_owner: USER,
            ..foreign_in_sticky_directory()
        };
        let root = Replacement {
            user: SUPERUSER,
            ..foreign_in_sticky_directory()
        };

        for replacement in [file_owner, directory_owner, root] {
            assert_eq!(replacement.refusal(), None, "{replacement:?}");
        }
    }

    #[test]
    fn test_another_users_file_without_the_sticky_bit_is_replaceable() {
        let foreign = Replacement {
            sticky: false,
            ..foreign_in_sticky_directory()
        };

        assert_eq!(foreign.refusal(), None);
    }

    #[test]
    fn test_nothing_is_replaced_when_the_destination_is_missing() {
        let directory = TempDir::new().unwrap();
        let created = NamedTempFile::new_in(directory.path()).unwrap();

        let replacement = Replacement::inspect(
            &directory.path().join(".env"),
            directory.path(),
            created.as_file(),
        )
        .unwrap();

        assert_eq!(replacement, None);
    }

    #[test]
    fn test_an_own_file_in_an_own_sticky_directory_is_inspected_as_replaceable() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TempDir::new().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o1700)).unwrap();
        let destination = directory.path().join(".env");
        fs::write(&destination, "EXISTING=value\n").unwrap();
        let created = NamedTempFile::new_in(directory.path()).unwrap();

        let replacement = Replacement::inspect(&destination, directory.path(), created.as_file())
            .unwrap()
            .unwrap();

        assert!(replacement.sticky, "{replacement:?}");
        assert!(!replacement.mounted, "{replacement:?}");
        assert_eq!(replacement.destination_owner, replacement.user);
        assert_eq!(replacement.refusal(), None);
    }
}

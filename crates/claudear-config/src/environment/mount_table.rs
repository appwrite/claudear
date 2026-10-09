use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::path::PathBuf;

const MOUNT_INFORMATION: &str = "/proc/self/mountinfo";
const MOUNT_POINT_FIELD: usize = 4;
const FIELD_SEPARATOR: char = ' ';
const ESCAPE: u8 = b'\\';
const OCTAL_DIGITS: usize = 3;
const OCTAL_RADIX: u32 = 8;

/// The mount points this process sees, as Linux lists them in
/// `/proc/self/mountinfo`. Empty where that file does not exist.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct MountTable {
    mount_points: Vec<PathBuf>,
}

impl MountTable {
    pub(super) fn read() -> Self {
        fs::read_to_string(MOUNT_INFORMATION)
            .map(|table| Self::parse(&table))
            .unwrap_or_default()
    }

    pub(super) fn contains(&self, path: &Path) -> bool {
        self.mount_points.iter().any(|point| point == path)
    }

    fn parse(table: &str) -> Self {
        Self {
            mount_points: table
                .lines()
                .filter_map(|line| line.split(FIELD_SEPARATOR).nth(MOUNT_POINT_FIELD))
                .map(Self::unescape)
                .collect(),
        }
    }

    fn unescape(field: &str) -> PathBuf {
        let bytes = field.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut index = 0;

        while index < bytes.len() {
            match Self::escaped_byte(&bytes[index..]) {
                Some(byte) => {
                    decoded.push(byte);
                    index += 1 + OCTAL_DIGITS;
                }
                None => {
                    decoded.push(bytes[index]);
                    index += 1;
                }
            }
        }

        PathBuf::from(OsString::from_vec(decoded))
    }

    fn escaped_byte(bytes: &[u8]) -> Option<u8> {
        let (&first, rest) = bytes.split_first()?;
        let digits = rest.get(..OCTAL_DIGITS)?;

        if first != ESCAPE || !digits.iter().all(|digit| (b'0'..=b'7').contains(digit)) {
            return None;
        }

        let value = digits.iter().fold(0, |value, digit| {
            value * OCTAL_RADIX + u32::from(digit - b'0')
        });
        u8::try_from(value).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = concat!(
        "22 1 259:2 / / rw,relatime shared:1 - ext4 /dev/root rw\n",
        "36 22 0:52 / /app rw,relatime - overlay overlay rw\n",
        "37 36 259:2 /srv/claudear/.env /app/.env rw,relatime - ext4 /dev/root rw\n",
        "38 36 259:2 /srv/my\\040secrets /app/my\\040secrets\\134.env rw - ext4 /dev/root rw\n",
    );

    #[test]
    fn test_a_single_file_bind_mount_is_a_mount_point() {
        let table = MountTable::parse(TABLE);

        assert!(table.contains(Path::new("/app/.env")));
        assert!(table.contains(Path::new("/app")));
        assert!(!table.contains(Path::new("/app/other.env")));
        assert!(!table.contains(Path::new("/srv/claudear/.env")));
    }

    #[test]
    fn test_escaped_spaces_and_backslashes_in_a_mount_point_are_decoded() {
        let table = MountTable::parse(TABLE);

        assert!(table.contains(Path::new("/app/my secrets\\.env")));
        assert!(!table.contains(Path::new("/app/my\\040secrets\\134.env")));
    }

    #[test]
    fn test_a_backslash_without_three_octal_digits_is_kept() {
        assert_eq!(MountTable::unescape("/a\\9b"), PathBuf::from("/a\\9b"));
        assert_eq!(MountTable::unescape("/a\\04"), PathBuf::from("/a\\04"));
        assert_eq!(MountTable::unescape("/a\\777"), PathBuf::from("/a\\777"));
    }

    #[test]
    fn test_an_empty_table_contains_nothing() {
        assert!(!MountTable::parse("").contains(Path::new("/")));
    }
}

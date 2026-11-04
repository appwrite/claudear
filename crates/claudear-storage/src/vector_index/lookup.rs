use super::declaration::Declaration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lookup {
    Absent,
    Declared(Declaration),
    Unreadable,
    Foreign,
}

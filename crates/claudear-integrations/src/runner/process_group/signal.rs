#[derive(Clone, Copy, Debug)]
pub(super) enum Signal {
    Interrupt,
    Kill,
}

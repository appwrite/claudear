#[cfg(not(unix))]
mod portable;
#[cfg(unix)]
mod unix;

#[cfg(not(unix))]
pub use portable::Signals;
#[cfg(unix)]
pub use unix::Signals;

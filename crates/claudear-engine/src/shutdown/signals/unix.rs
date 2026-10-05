use crate::shutdown::Reason;
use futures::Stream;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::signal::unix::{self, Signal, SignalKind};

/// The signals that stop the daemon, each with the reason it reads as.
const STOP_SIGNALS: [(SignalKind, Reason); 3] = [
    (SignalKind::interrupt(), Reason::Interrupted),
    (SignalKind::terminate(), Reason::Terminated),
    (SignalKind::hangup(), Reason::Interrupted),
];

/// The stop signals, as a stream of shutdown reasons: SIGINT and SIGHUP read as
/// [`Reason::Interrupted`], SIGTERM as [`Reason::Terminated`].
///
/// A signal already ignored at startup stays ignored, as whoever started the daemon intended:
/// `nohup` ignores SIGHUP so the daemon outlives its terminal, and a script's background jobs
/// ignore SIGINT so Ctrl+C leaves them running.
#[derive(Debug)]
pub struct Signals {
    listeners: Vec<(Signal, Reason)>,
}

impl Signals {
    /// Installs the handlers inside the current Tokio runtime. From then on these signals no
    /// longer kill the process, so the caller decides how to exit.
    pub fn listen() -> io::Result<Self> {
        let mut listeners = Vec::with_capacity(STOP_SIGNALS.len());
        for (kind, reason) in heeded(disposition)? {
            listeners.push((unix::signal(kind)?, reason));
        }
        Ok(Self { listeners })
    }
}

impl Stream for Signals {
    type Item = Reason;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Reason>> {
        receive(&mut self.listeners, |listener| listener.poll_recv(context))
    }
}

/// Yields the reason of the first listener whose signal arrived.
///
/// A closed listener can never deliver its signal, so it is dropped rather than taken for one,
/// and the signals end once no listener is left.
fn receive<Listener>(
    listeners: &mut Vec<(Listener, Reason)>,
    mut poll: impl FnMut(&mut Listener) -> Poll<Option<()>>,
) -> Poll<Option<Reason>> {
    let mut index = 0;
    while let Some((listener, reason)) = listeners.get_mut(index) {
        match poll(listener) {
            Poll::Ready(Some(())) => return Poll::Ready(Some(*reason)),
            Poll::Ready(None) => {
                listeners.remove(index);
            }
            Poll::Pending => index += 1,
        }
    }
    if listeners.is_empty() {
        Poll::Ready(None)
    } else {
        Poll::Pending
    }
}

/// The stop signals worth listening for: those `disposition` does not report as ignored.
fn heeded(
    disposition: impl Fn(libc::c_int) -> io::Result<libc::sighandler_t>,
) -> io::Result<Vec<(SignalKind, Reason)>> {
    let mut heeded = Vec::with_capacity(STOP_SIGNALS.len());
    for (kind, reason) in STOP_SIGNALS {
        if disposition(kind.as_raw_value())? != libc::SIG_IGN {
            heeded.push((kind, reason));
        }
    }
    Ok(heeded)
}

/// Reads the handler currently installed for `signal` without changing it.
fn disposition(signal: libc::c_int) -> io::Result<libc::sighandler_t> {
    // SAFETY: every field of `sigaction` is an integer, a pointer or an optional function
    // pointer, so all-zero bytes are a valid value.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: with a null new action, sigaction changes nothing and only writes the current
    // action into `action`.
    if unsafe { libc::sigaction(signal, std::ptr::null(), &mut action) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(action.sa_sigaction)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDLE: Poll<Option<()>> = Poll::Pending;
    const DELIVERED: Poll<Option<()>> = Poll::Ready(Some(()));
    const CLOSED: Poll<Option<()>> = Poll::Ready(None);

    fn received(listeners: &mut Vec<(Poll<Option<()>>, Reason)>) -> Poll<Option<Reason>> {
        receive(listeners, |listener| *listener)
    }

    #[test]
    fn delivered_signal_yields_its_reason() {
        let mut listeners = vec![(IDLE, Reason::Interrupted), (DELIVERED, Reason::Terminated)];

        assert_eq!(
            received(&mut listeners),
            Poll::Ready(Some(Reason::Terminated))
        );
        assert_eq!(listeners.len(), 2, "every listener keeps listening");
    }

    #[test]
    fn closed_listener_is_dropped_instead_of_read_as_a_signal() {
        let mut listeners = vec![(CLOSED, Reason::Interrupted), (IDLE, Reason::Terminated)];

        assert_eq!(received(&mut listeners), Poll::Pending);
        assert_eq!(listeners, [(IDLE, Reason::Terminated)]);
    }

    #[test]
    fn signal_behind_a_closed_listener_is_still_delivered() {
        let mut listeners = vec![
            (CLOSED, Reason::Interrupted),
            (DELIVERED, Reason::Terminated),
        ];

        assert_eq!(
            received(&mut listeners),
            Poll::Ready(Some(Reason::Terminated))
        );
    }

    #[test]
    fn signals_end_once_every_listener_has_closed() {
        let mut listeners = vec![(CLOSED, Reason::Interrupted), (CLOSED, Reason::Terminated)];

        assert_eq!(received(&mut listeners), Poll::Ready(None));
        assert!(listeners.is_empty());
    }

    #[test]
    fn signals_without_listeners_have_ended() {
        assert_eq!(received(&mut Vec::new()), Poll::Ready(None));
    }

    #[test]
    fn unreadable_disposition_fails_listening() {
        let error = heeded(|_| Err(io::Error::from_raw_os_error(libc::EINVAL)))
            .expect_err("a disposition that cannot be read fails listening");

        assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
    }

    #[test]
    fn disposition_reads_an_ignored_signal_as_ignored() {
        let handler = disposition(libc::SIGPIPE).expect("SIGPIPE is a valid signal");

        assert_eq!(
            handler,
            libc::SIG_IGN,
            "the Rust runtime ignores SIGPIPE before main"
        );
    }

    #[test]
    fn disposition_rejects_an_invalid_signal() {
        let error = disposition(0).expect_err("0 is not a signal sigaction accepts");

        assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
    }
}

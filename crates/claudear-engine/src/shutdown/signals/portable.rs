use crate::shutdown::Reason;
use futures::stream::{self, BoxStream, Stream, StreamExt};
use std::fmt;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Ctrl+C, the one stop signal every platform delivers, as a stream of
/// [`Reason::Interrupted`].
pub struct Signals {
    interrupts: BoxStream<'static, Reason>,
}

impl Signals {
    /// Listens for Ctrl+C inside the current Tokio runtime. From the first poll on, Ctrl+C no
    /// longer kills the process, so the caller decides how to exit.
    pub fn listen() -> io::Result<Self> {
        let interrupts = stream::unfold((), |()| async {
            tokio::signal::ctrl_c()
                .await
                .inspect_err(|error| tracing::warn!("Could not listen for Ctrl+C: {error}"))
                .ok()?;
            Some((Reason::Interrupted, ()))
        });
        Ok(Self {
            interrupts: interrupts.fuse().boxed(),
        })
    }
}

impl Stream for Signals {
    type Item = Reason;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Reason>> {
        self.interrupts.poll_next_unpin(context)
    }
}

impl fmt::Debug for Signals {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Signals").finish_non_exhaustive()
    }
}

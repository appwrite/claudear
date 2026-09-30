use futures::future::{FutureExt, LocalBoxFuture};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A long-running part of the daemon, such as the IPC server or the poll loop.
pub struct Service<'a> {
    name: &'static str,
    future: LocalBoxFuture<'a, anyhow::Result<()>>,
}

impl<'a> Service<'a> {
    /// Wraps `future` under `name`, which the shutdown log and
    /// [`Reason::ServiceEnded`](super::Reason::ServiceEnded) report.
    pub fn new(name: &'static str, future: impl Future<Output = anyhow::Result<()>> + 'a) -> Self {
        Self {
            name,
            future: future.boxed_local(),
        }
    }
}

impl Future for Service<'_> {
    type Output = (&'static str, anyhow::Result<()>);

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let name = self.name;
        self.future.poll_unpin(context).map(|result| (name, result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolves_to_its_name_and_result() {
        let (name, result) = Service::new("ipc", async { Err(anyhow::anyhow!("closed")) }).await;

        assert_eq!(name, "ipc");
        assert_eq!(result.unwrap_err().to_string(), "closed");
    }
}

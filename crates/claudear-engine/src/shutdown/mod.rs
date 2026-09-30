//! Graceful shutdown for the daemon: stop taking new work, then drain in-flight runs.

mod outcome;
mod reason;
mod service;
mod signals;
mod summary;

pub use outcome::Outcome;
pub use reason::Reason;
pub use service::Service;
pub use signals::Signals;
pub use summary::Summary;

use futures::stream::{self, FuturesUnordered, Stream, StreamExt};
use std::future::Future;
use std::pin::pin;
use std::time::Duration;

/// How long shutdown waits for in-flight runs to finish.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long interrupted agent CLIs get to end, and their runs to record how they ended, before
/// whatever is left of them is killed.
pub const INTERRUPT_GRACE: Duration = Duration::from_secs(5);

/// How long the runtime waits for blocking work, such as a local model call, after the drain
/// before the process exits anyway.
pub const RUNTIME_GRACE: Duration = Duration::from_secs(5);

/// How long `claudear stop` waits for the daemon to exit: the drain, the agent CLIs' interrupt
/// grace and the runtime grace, plus a margin for the process to finish exiting.
pub const EXIT_TIMEOUT: Duration = DRAIN_TIMEOUT
    .saturating_add(INTERRUPT_GRACE)
    .saturating_add(RUNTIME_GRACE)
    .saturating_add(EXIT_MARGIN);

const EXIT_MARGIN: Duration = Duration::from_secs(5);

const FORCE_QUIT_HINT: &str = "Press Ctrl+C again to force quit.";

/// Runs `services` until a signal, `request` or the end of a service starts the shutdown, then
/// calls `begin` with the reason and waits up to [`DRAIN_TIMEOUT`] for `drain` to report that
/// in-flight runs finished, while the remaining services keep running.
///
/// Another signal during the drain forces the shutdown, and a signal stream that ends counts as
/// silence. A service that ends before anything asked the daemon to stop fails the shutdown even
/// when it returns `Ok`, since services only return on their own when something broke. `run`
/// never exits the process: the caller picks the exit from the [`Summary`].
pub async fn run<'a>(
    services: impl IntoIterator<Item = Service<'a>>,
    request: impl Future<Output = ()>,
    signals: &mut (impl Stream<Item = Reason> + Unpin),
    begin: impl FnOnce(Reason),
    drain: impl Future<Output = bool>,
) -> Summary {
    let mut services: FuturesUnordered<Service<'a>> = services.into_iter().collect();
    let mut signals = signals.chain(stream::pending());
    let (reason, error) = serve(&mut services, request, &mut signals).await;
    tracing::warn!(reason = reason.label(), "{}", notice(reason));
    begin(reason);
    let outcome = finish(&mut services, &mut signals, drain).await;
    Summary {
        reason,
        outcome,
        error,
    }
}

async fn serve(
    services: &mut FuturesUnordered<Service<'_>>,
    request: impl Future<Output = ()>,
    signals: &mut (impl Stream<Item = Reason> + Unpin),
) -> (Reason, Option<anyhow::Error>) {
    tokio::select! {
        biased;
        Some(reason) = signals.next() => (reason, None),
        () = request => (Reason::Requested, None),
        Some((name, result)) = services.next(), if !services.is_empty() => {
            (Reason::ServiceEnded(name), Some(ended_early(name, result)))
        }
    }
}

async fn finish(
    services: &mut FuturesUnordered<Service<'_>>,
    signals: &mut (impl Stream<Item = Reason> + Unpin),
    drain: impl Future<Output = bool>,
) -> Outcome {
    let mut drain = pin!(tokio::time::timeout(DRAIN_TIMEOUT, drain));
    loop {
        tokio::select! {
            biased;
            Some(_) = signals.next() => return Outcome::Forced,
            drained = &mut drain => {
                return match drained {
                    Ok(true) => Outcome::Drained,
                    Ok(false) | Err(_) => Outcome::TimedOut,
                };
            }
            Some((name, result)) = services.next(), if !services.is_empty() => {
                ended_during_drain(name, result);
            }
        }
    }
}

fn notice(reason: Reason) -> String {
    let seconds = DRAIN_TIMEOUT.as_secs();
    let message =
        format!("Shutting down ({reason}): draining in-flight runs for up to {seconds}s.");
    if reason == Reason::Interrupted {
        format!("{message} {FORCE_QUIT_HINT}")
    } else {
        message
    }
}

fn ended_early(name: &'static str, result: anyhow::Result<()>) -> anyhow::Error {
    match result {
        Ok(()) => {
            let error = anyhow::anyhow!("The {name} service stopped unexpectedly");
            tracing::error!(service = name, "{error}");
            error
        }
        Err(error) => {
            tracing::error!(service = name, "The {name} service failed: {error:#}");
            error
        }
    }
}

fn ended_during_drain(name: &'static str, result: anyhow::Result<()>) {
    match result {
        Ok(()) => tracing::debug!(service = name, "The {name} service stopped"),
        Err(error) => {
            tracing::error!(
                service = name,
                "The {name} service failed while draining: {error:#}"
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::channel::mpsc::unbounded;
    use std::cell::{Cell, RefCell};
    use std::future::{pending, poll_fn, ready};
    use std::task::Poll;
    use tokio::sync::{broadcast, oneshot};
    use tokio::time::{sleep, timeout, Instant};

    const RUN_TIME: Duration = Duration::from_secs(5);
    const DELAY: Duration = Duration::from_secs(1);
    const POLL_LIMIT: usize = 16;

    fn idle(name: &'static str) -> Service<'static> {
        Service::new(name, pending())
    }

    async fn finished_after(duration: Duration) -> bool {
        sleep(duration).await;
        true
    }

    fn limited<Output>(future: impl Future<Output = Output>) -> impl Future<Output = Output> {
        let mut future = Box::pin(future);
        let mut polls: usize = 0;
        poll_fn(move |context| {
            polls += 1;
            assert!(
                polls <= POLL_LIMIT,
                "polled {polls} times: something busy-loops"
            );
            future.as_mut().poll(context)
        })
    }

    fn guarded(
        mut signals: impl Stream<Item = Reason> + Unpin,
    ) -> impl Stream<Item = Reason> + Unpin {
        let mut ended = false;
        stream::poll_fn(move |context| {
            assert!(!ended, "the signal stream was polled again after it ended");
            let next = signals.poll_next_unpin(context);
            ended = matches!(next, Poll::Ready(None));
            next
        })
    }

    #[tokio::test(start_paused = true)]
    async fn service_ending_right_after_the_request_does_not_cut_the_drain_short() {
        let (_sender, mut signals) = unbounded::<Reason>();
        let (stop, mut accepted) = broadcast::channel::<()>(1);
        let mut requested = stop.subscribe();
        stop.send(()).expect("both receivers are subscribed");
        let ipc = Service::new("ipc", async move {
            accepted.recv().await?;
            Ok(())
        });
        let drained = Cell::new(false);
        let started = Instant::now();

        let summary = run(
            [ipc, idle("poll")],
            async move {
                let _ = requested.recv().await;
            },
            &mut signals,
            |_| {},
            async {
                sleep(RUN_TIME).await;
                drained.set(true);
                true
            },
        )
        .await;

        assert_eq!(summary.reason, Reason::Requested);
        assert_eq!(summary.outcome, Outcome::Drained);
        assert!(summary.error.is_none());
        assert!(drained.get(), "the drain must run to completion");
        assert_eq!(started.elapsed(), RUN_TIME);
    }

    #[tokio::test(start_paused = true)]
    async fn service_failure_starts_the_shutdown_and_is_reported() {
        let (_sender, mut signals) = unbounded::<Reason>();
        let http = Service::new("http", async { Err(anyhow::anyhow!("address in use")) });
        let begun = Cell::new(None);
        let drained = Cell::new(false);

        let summary = run(
            [http, idle("poll")],
            pending(),
            &mut signals,
            |reason| begun.set(Some(reason)),
            async {
                sleep(RUN_TIME).await;
                drained.set(true);
                true
            },
        )
        .await;

        assert_eq!(begun.get(), Some(Reason::ServiceEnded("http")));
        assert_eq!(summary.reason, Reason::ServiceEnded("http"));
        assert_eq!(summary.outcome, Outcome::Drained);
        assert!(drained.get(), "the drain must still be awaited");
        let error = summary.error.expect("the service error is reported");
        assert_eq!(error.to_string(), "address in use");
    }

    #[tokio::test(start_paused = true)]
    async fn service_stopping_on_its_own_starts_the_shutdown_and_is_reported_as_an_error() {
        let (_sender, mut signals) = unbounded::<Reason>();
        let drained = Cell::new(false);

        let summary = run(
            [Service::new("poll", async { Ok(()) }), idle("ipc")],
            pending(),
            &mut signals,
            |_| {},
            async {
                sleep(RUN_TIME).await;
                drained.set(true);
                true
            },
        )
        .await;

        assert_eq!(summary.reason, Reason::ServiceEnded("poll"));
        assert_eq!(summary.outcome, Outcome::Drained);
        assert!(drained.get(), "the drain must still run to completion");
        let error = summary
            .error
            .expect("a service that stops on its own fails the shutdown");
        assert_eq!(error.to_string(), "The poll service stopped unexpectedly");
    }

    #[tokio::test(start_paused = true)]
    async fn second_signal_during_the_drain_forces_the_shutdown() {
        let (sender, mut signals) = unbounded();
        sender.unbounded_send(Reason::Interrupted).unwrap();
        let started = Instant::now();

        let (summary, ()) = tokio::join!(
            run([idle("poll")], pending(), &mut signals, |_| {}, pending()),
            async {
                sleep(DELAY).await;
                sender.unbounded_send(Reason::Interrupted).unwrap();
            },
        );

        assert_eq!(summary.reason, Reason::Interrupted);
        assert_eq!(summary.outcome, Outcome::Forced);
        assert_eq!(started.elapsed(), DELAY);
    }

    #[tokio::test(start_paused = true)]
    async fn signal_after_a_requested_stop_forces_the_shutdown() {
        let (sender, mut signals) = unbounded();

        let (summary, ()) = tokio::join!(
            run([idle("poll")], ready(()), &mut signals, |_| {}, pending()),
            async {
                sleep(DELAY).await;
                sender.unbounded_send(Reason::Terminated).unwrap();
            },
        );

        assert_eq!(summary.reason, Reason::Requested);
        assert_eq!(summary.outcome, Outcome::Forced);
    }

    #[tokio::test(start_paused = true)]
    async fn services_keep_running_during_the_drain() {
        let (_sender, mut signals) = unbounded::<Reason>();
        let (begun, begun_receiver) = oneshot::channel::<()>();
        let (finished, finished_receiver) = oneshot::channel::<()>();
        let worker = Service::new("worker", async move {
            begun_receiver.await?;
            let _ = finished.send(());
            pending().await
        });

        let summary = run(
            [worker],
            ready(()),
            &mut signals,
            move |_| {
                let _ = begun.send(());
            },
            async { finished_receiver.await.is_ok() },
        )
        .await;

        assert_eq!(summary.outcome, Outcome::Drained);
    }

    #[tokio::test(start_paused = true)]
    async fn drain_that_never_finishes_times_out_at_the_drain_timeout() {
        let (_sender, mut signals) = unbounded::<Reason>();
        let started = Instant::now();

        let summary = run([idle("poll")], ready(()), &mut signals, |_| {}, pending()).await;

        assert_eq!(summary.outcome, Outcome::TimedOut);
        assert_eq!(started.elapsed(), DRAIN_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn run_returns_once_the_drain_finishes_while_services_stay_pending() {
        let (_sender, mut signals) = unbounded::<Reason>();
        let started = Instant::now();

        let summary = run(
            [idle("ipc"), idle("http")],
            ready(()),
            &mut signals,
            |_| {},
            finished_after(RUN_TIME),
        )
        .await;

        assert_eq!(summary.outcome, Outcome::Drained);
        assert_eq!(started.elapsed(), RUN_TIME);
    }

    #[tokio::test(start_paused = true)]
    async fn begin_runs_once_with_the_reason_before_the_drain_is_polled() {
        for (signal, expected) in [
            (Some(Reason::Interrupted), Reason::Interrupted),
            (Some(Reason::Terminated), Reason::Terminated),
            (None, Reason::Requested),
        ] {
            let (sender, mut signals) = unbounded();
            if let Some(reason) = signal {
                sender.unbounded_send(reason).unwrap();
            }
            let begun = RefCell::new(Vec::new());
            let seen = RefCell::new(Vec::new());

            let summary = run(
                [idle("poll")],
                async {
                    if signal.is_some() {
                        pending::<()>().await;
                    }
                },
                &mut signals,
                |reason| begun.borrow_mut().push(reason),
                async {
                    seen.replace(begun.borrow().clone());
                    true
                },
            )
            .await;

            assert_eq!(
                seen.into_inner(),
                [expected],
                "begin runs once, before the drain"
            );
            assert_eq!(begun.into_inner(), [expected]);
            assert_eq!(summary.reason, expected);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn drain_that_gives_up_counts_as_timed_out() {
        let (_sender, mut signals) = unbounded::<Reason>();
        let started = Instant::now();

        let summary = run(
            [idle("poll")],
            ready(()),
            &mut signals,
            |_| {},
            ready(false),
        )
        .await;

        assert_eq!(summary.outcome, Outcome::TimedOut);
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn signal_stream_ending_before_the_stop_does_not_stop_the_daemon() {
        let (sender, receiver) = unbounded::<Reason>();
        drop(sender);
        let mut signals = guarded(receiver);
        let started = Instant::now();

        let summary = run(
            [idle("poll")],
            sleep(DELAY),
            &mut signals,
            |_| {},
            ready(true),
        )
        .await;

        assert_eq!(summary.reason, Reason::Requested);
        assert_eq!(summary.outcome, Outcome::Drained);
        assert_eq!(started.elapsed(), DELAY);
    }

    #[tokio::test(start_paused = true)]
    async fn signal_stream_ending_during_the_drain_neither_forces_nor_spins() {
        let (sender, receiver) = unbounded();
        sender.unbounded_send(Reason::Interrupted).unwrap();
        drop(sender);
        let mut signals = guarded(receiver);
        let started = Instant::now();

        let summary = timeout(
            DRAIN_TIMEOUT * 2,
            run([idle("poll")], pending(), &mut signals, |_| {}, pending()),
        )
        .await
        .expect("run ends at the drain timeout");

        assert_eq!(summary.reason, Reason::Interrupted);
        assert_eq!(summary.outcome, Outcome::TimedOut);
        assert_eq!(started.elapsed(), DRAIN_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn service_failure_during_the_drain_is_not_reported() {
        let (_sender, mut signals) = unbounded::<Reason>();
        let (begun, begun_receiver) = oneshot::channel::<()>();
        let failed = Cell::new(false);
        let http = Service::new("http", async {
            begun_receiver.await?;
            failed.set(true);
            Err(anyhow::anyhow!("connection reset"))
        });

        let summary = run(
            [http],
            ready(()),
            &mut signals,
            move |_| {
                let _ = begun.send(());
            },
            finished_after(RUN_TIME),
        )
        .await;

        assert!(failed.get(), "the service must fail during the drain");
        assert_eq!(summary.outcome, Outcome::Drained);
        assert!(summary.error.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn second_signal_wins_over_a_drain_that_is_also_ready() {
        let (sender, mut signals) = unbounded();
        sender.unbounded_send(Reason::Terminated).unwrap();
        sender.unbounded_send(Reason::Interrupted).unwrap();

        let summary = run([idle("poll")], pending(), &mut signals, |_| {}, ready(true)).await;

        assert_eq!(summary.reason, Reason::Terminated);
        assert_eq!(summary.outcome, Outcome::Forced);
    }

    #[tokio::test(start_paused = true)]
    async fn no_services_still_stop_and_drain_without_spinning() {
        let (_sender, mut signals) = unbounded::<Reason>();
        let started = Instant::now();

        let summary = run(
            [],
            limited(sleep(DELAY)),
            &mut signals,
            |_| {},
            limited(finished_after(RUN_TIME)),
        )
        .await;

        assert_eq!(summary.reason, Reason::Requested);
        assert_eq!(summary.outcome, Outcome::Drained);
        assert_eq!(started.elapsed(), DELAY + RUN_TIME);
    }

    #[tokio::test(start_paused = true)]
    async fn signal_wins_over_a_request_that_is_also_ready() {
        let (sender, mut signals) = unbounded();
        sender.unbounded_send(Reason::Interrupted).unwrap();

        let summary = run([idle("poll")], ready(()), &mut signals, |_| {}, ready(true)).await;

        assert_eq!(summary.reason, Reason::Interrupted);
        assert_eq!(summary.outcome, Outcome::Drained);
    }

    #[test]
    fn notice_names_the_reason_and_offers_force_quit_only_when_interrupted() {
        let seconds = format!("{}s", DRAIN_TIMEOUT.as_secs());
        for reason in [
            Reason::Requested,
            Reason::Interrupted,
            Reason::Terminated,
            Reason::ServiceEnded("ipc"),
        ] {
            let line = notice(reason);

            assert!(line.contains(&reason.to_string()), "{line}");
            assert!(line.contains(&seconds), "{line}");
            assert_eq!(
                line.contains(FORCE_QUIT_HINT),
                reason == Reason::Interrupted,
                "{line}"
            );
        }
    }
}

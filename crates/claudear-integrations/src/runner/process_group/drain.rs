use std::future::Future;
use std::io;
use std::pin::Pin;
use tokio::io::{AsyncBufRead, Lines};

type Deadline = Pin<Box<dyn Future<Output = ()> + Send>>;

/// How long readers keep reading an agent CLI's output once its process group
/// is killed. The CLI's own output is buffered by then, so a reader takes all of
/// it and stops once the pipe is empty after the grace: a process that moved to
/// a session of its own escaped the kill and can hold the pipe open forever. At
/// the cutoff it stops even while such a process keeps writing.
pub struct Drain {
    grace: Deadline,
    cutoff: Deadline,
    over: bool,
}

impl Drain {
    pub(super) fn new(
        grace: impl Future<Output = ()> + Send + 'static,
        cutoff: impl Future<Output = ()> + Send + 'static,
    ) -> Self {
        Self {
            grace: Box::pin(grace),
            cutoff: Box::pin(cutoff),
            over: false,
        }
    }

    /// The next line of `lines`, or `None` once the drain is over.
    pub async fn next_line<R: AsyncBufRead + Unpin>(
        &mut self,
        lines: &mut Lines<R>,
    ) -> Option<io::Result<Option<String>>> {
        if self.over {
            return None;
        }
        let line = tokio::select! {
            biased;
            () = &mut self.cutoff => None,
            line = lines.next_line() => Some(line),
            () = &mut self.grace => None,
        };
        self.over = line.is_none();
        line
    }
}

#[cfg(all(test, unix))]
mod tests {
    use crate::runner::process_group::tests::{
        group_command, kill, recorded_escaped_pid, ESCAPED_PROCESS, EXIT_DEADLINE,
    };
    use crate::runner::process_group::{Guard, Registry};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, BufReader};

    /// Like [`ESCAPED_PROCESS`], but writing to the stub's stdout as fast as the
    /// pipe takes it.
    const ESCAPED_WRITER: &str = concat!(
        r#"perl -e 'use POSIX; setsid(); open(my $f, ">", "escaped") or die; "#,
        r#"print $f "$$\n"; close $f; print "noise\n" while 1' &"#,
        "\nwhile [ ! -s escaped ]; do sleep 0.1; done\n",
    );

    const WRITTEN_LINES: usize = 500;

    /// Per line, so the reader falls behind whatever writes to the pipe.
    const READ_DELAY: Duration = Duration::from_millis(1);

    #[tokio::test]
    async fn test_drain_gives_a_reader_that_fell_behind_everything_the_cli_wrote() {
        let directory = tempfile::tempdir().unwrap();
        let registry = Registry::new();
        let script = format!(
            "{ESCAPED_PROCESS}i=0; while [ $i -lt {WRITTEN_LINES} ]; do echo line; i=$((i + 1)); done\n"
        );
        let (mut leader, mut guard) = Guard::spawn(
            group_command(&script).current_dir(directory.path()),
            &registry,
        )
        .unwrap();
        let mut lines = BufReader::new(leader.stdout.take().unwrap()).lines();
        let mut drain = guard.drain(Duration::ZERO, EXIT_DEADLINE);

        guard.wait(&mut leader).await.unwrap();
        let read = tokio::time::timeout(EXIT_DEADLINE, async {
            let mut read = 0;
            while let Some(Ok(Some(_))) = drain.next_line(&mut lines).await {
                read += 1;
                tokio::time::sleep(READ_DELAY).await;
            }
            read
        })
        .await;
        kill(recorded_escaped_pid(directory.path()).expect("the stub records the escaped pid"));

        let read = read.expect("the drain must end once the pipe is empty");
        assert_eq!(
            read, WRITTEN_LINES,
            "output the CLI wrote before its group was killed must be read after the grace"
        );
    }

    #[tokio::test]
    async fn test_drain_ends_at_the_cutoff_while_an_escaped_process_keeps_writing() {
        let directory = tempfile::tempdir().unwrap();
        let registry = Registry::new();
        let (mut leader, mut guard) = Guard::spawn(
            group_command(ESCAPED_WRITER).current_dir(directory.path()),
            &registry,
        )
        .unwrap();
        let mut lines = BufReader::new(leader.stdout.take().unwrap()).lines();
        let mut drain = guard.drain(Duration::ZERO, Duration::from_secs(1));

        guard.wait(&mut leader).await.unwrap();
        let drained = tokio::time::timeout(EXIT_DEADLINE, async {
            while drain.next_line(&mut lines).await.is_some() {
                tokio::time::sleep(READ_DELAY).await;
            }
        })
        .await;
        kill(recorded_escaped_pid(directory.path()).expect("the stub records the escaped pid"));

        assert!(
            drained.is_ok(),
            "the drain must end at its cutoff while a process outside the group keeps writing"
        );
    }
}

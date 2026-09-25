//! Agent CLIs lead process groups of their own, so a run can kill whatever it
//! leaves behind and claudear can reach every CLI when it shuts down.

mod guard;
mod registry;
mod signal;

pub use guard::Guard;
pub use registry::Registry;

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::{Child, Command};

    /// Generous, because CI runs the tests under `cargo tarpaulin`'s ptrace.
    pub(crate) const EXIT_DEADLINE: Duration = Duration::from_secs(30);

    /// Generous, because CI runs the tests under `cargo tarpaulin`'s ptrace.
    pub(crate) const RUN_DEADLINE: Duration = Duration::from_secs(60);

    pub(crate) const BACKGROUND_SLEEP: &str = "sleep 300 & echo $!; exec sleep 300";

    /// Stub CLI setup that leaves `sleep 300` running in the background,
    /// holding the stub's stdout, and records "<background pid> <stub pid>" in
    /// `pids`.
    pub(crate) const BACKGROUND_PROCESS: &str = "sleep 300 &\necho \"$! $$\" > pids\n";

    /// Stub CLI setup that leaves `sleep 300` running in a session of its own,
    /// beyond the reach of a process group kill, holding the stub's stdout. It
    /// records its pid in `escaped` once it has left the group.
    pub(crate) const ESCAPED_PROCESS: &str = concat!(
        r#"perl -e 'use POSIX; setsid(); open(my $f, ">", "escaped") or die; "#,
        r#"print $f "$$\n"; close $f; sleep 300' &"#,
        "\nwhile [ ! -s escaped ]; do sleep 0.1; done\n",
    );

    const STUB_PROBE: &str = "--probe";

    /// Write `binary` as a stub CLI running `script` with `sh`, returning once
    /// it can be executed.
    pub(crate) fn install_stub(binary: &Path, script: &str) {
        let stub = format!("#!/bin/sh\n[ \"$1\" = {STUB_PROBE} ] && exit 0\n{script}");
        std::fs::write(binary, stub).unwrap();
        std::fs::set_permissions(binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        wait_until_executable(binary);
    }

    /// Exec `binary` until the kernel stops refusing it with ETXTBSY: a process
    /// a concurrent test forks can briefly inherit the descriptor it was
    /// written through.
    fn wait_until_executable(binary: &Path) {
        let start = Instant::now();
        loop {
            let probe = std::process::Command::new(binary)
                .arg(STUB_PROBE)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            match probe {
                Ok(_) => return,
                Err(error)
                    if error.raw_os_error() == Some(libc::ETXTBSY)
                        && start.elapsed() < RUN_DEADLINE =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("the stub CLI must be executable: {error}"),
            }
        }
    }

    /// The `(background, stub)` pids [`BACKGROUND_PROCESS`] recorded.
    pub(crate) fn recorded_pids(directory: &Path) -> Option<(u32, u32)> {
        let contents = std::fs::read_to_string(directory.join("pids")).ok()?;
        let (background, stub) = contents.strip_suffix('\n')?.split_once(' ')?;
        Some((background.parse().ok()?, stub.parse().ok()?))
    }

    pub(crate) async fn wait_for_recorded_pids(directory: &Path) -> (u32, u32) {
        loop {
            if let Some(pids) = recorded_pids(directory) {
                return pids;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The pid [`ESCAPED_PROCESS`] recorded.
    pub(crate) fn recorded_escaped_pid(directory: &Path) -> Option<u32> {
        let contents = std::fs::read_to_string(directory.join("escaped")).ok()?;
        contents.strip_suffix('\n')?.parse().ok()
    }

    /// Like [`BACKGROUND_SLEEP`], but the leader exits with status 7 on SIGINT
    /// while its background process ignores it. Perl, because a shell cannot
    /// trap a signal it inherited as ignored.
    pub(crate) const INTERRUPTIBLE_LEADER: &str = concat!(
        r#"exec perl -e '$| = 1; $SIG{INT} = sub { exit 7 }; "#,
        r#"defined(my $pid = fork) or die; "#,
        r#"if (!$pid) { $SIG{INT} = "IGNORE"; exec "sleep", "300" } "#,
        r#"print "$pid\n"; sleep 300'"#,
    );

    /// A shell running `script`, which must print the pid of a background
    /// process first.
    pub(crate) fn group_command(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]).stdout(Stdio::piped());
        command
    }

    pub(crate) async fn background_pid(leader: &mut Child) -> u32 {
        let mut line = String::new();
        BufReader::new(leader.stdout.take().unwrap())
            .read_line(&mut line)
            .await
            .unwrap();
        line.trim().parse().unwrap()
    }

    /// Spawn `script` leading a process group of its own, without a guard.
    pub(crate) async fn spawn_group(script: &str) -> (Child, u32) {
        let mut leader = group_command(script)
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let background = background_pid(&mut leader).await;
        (leader, background)
    }

    /// Zombies count as exited: whoever reaps them may never get to it (e.g. a
    /// container without an init process).
    pub(crate) fn is_running(pid: u32) -> bool {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&output.stdout);
        let stat = stat.trim();
        !stat.is_empty() && !stat.starts_with('Z')
    }

    pub(crate) fn exits_within(pid: u32) -> bool {
        assert!(
            is_running(std::process::id()),
            "ps must report live processes, or every exit check passes"
        );
        let start = Instant::now();
        while start.elapsed() < EXIT_DEADLINE {
            if !is_running(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        !is_running(pid)
    }

    pub(crate) async fn assert_killed(leader: &mut Child) {
        let status = tokio::time::timeout(EXIT_DEADLINE, leader.wait())
            .await
            .expect("the leader outlived its group's kill")
            .unwrap();
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "the leader exited with {status} instead of being killed"
        );
    }

    pub(crate) async fn assert_interrupted(leader: &mut Child) {
        let status = tokio::time::timeout(EXIT_DEADLINE, leader.wait())
            .await
            .expect("the leader outlived its interrupt")
            .unwrap();
        assert_eq!(status.code(), Some(7), "the leader exited with {status}");
    }

    pub(crate) fn kill(pid: u32) {
        // SAFETY: kill takes no pointers and only sends a signal.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
}

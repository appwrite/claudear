#![cfg(all(unix, feature = "sqlite"))]

mod sandbox;

use claudear::ipc::{IpcClient, IpcData, IpcResponse};
use sandbox::Sandbox;
use std::fs::{self, File};
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixListener;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout, Instant};

const STARTUP_WAIT: Duration = Duration::from_secs(40);
const EXIT_WAIT: Duration = Duration::from_secs(20);
const HANGUP_WAIT: Duration = Duration::from_secs(2);
const SUSPEND_WAIT: Duration = Duration::from_secs(5);
const HTTP_CHECK_WAIT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const DAEMON_POLL_INTERVAL: Duration = Duration::from_secs(3600);
const STOP_SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];
const DAEMON: &str = "daemon";
const DETACHED: &str = "detached";
const WEBHOOK: &str = "webhook";
const POLL: &str = "poll";
const ACTION: &str = "action";
const STOP: &str = "stop";
const STOPPED: &str = "Daemon stopped.";
const ALREADY_RUNNING: &str = "Another claudear daemon is already running";
const FORCED_NOTICE: &str = "Shutdown forced";
const ISSUE_KEY: &str = "SANDBOX-1";

/// The Jira issue [`ISSUE_KEY`], as Jira's REST API returns it.
const ISSUE: &str = r#"{
    "id": "10001",
    "key": "SANDBOX-1",
    "self": "http://jira.invalid/rest/api/3/issue/10001",
    "fields": {
        "summary": "How do I export my data?",
        "status": {"name": "To Do", "statusCategory": {"key": "new", "name": "To Do"}},
        "project": {"key": "SANDBOX", "name": "Sandbox"}
    }
}"#;

impl Sandbox {
    /// Claudear with `arguments`, with the stop signals at their default disposition: claudear
    /// keeps ignoring any it inherits ignored, and a test runner that a non-interactive shell
    /// starts in the background inherits SIGINT ignored.
    fn claudear(&self, name: &str, arguments: &[&str]) -> Command {
        let mut command = self.command(name, arguments);
        // SAFETY: the hook runs between fork and exec and only calls set_disposition, which is
        // async-signal-safe.
        unsafe { command.pre_exec(|| set_disposition(&STOP_SIGNALS, libc::SIG_DFL)) };
        command
    }

    /// The daemon's command: `claudear start` in the foreground, polling but serving no HTTP.
    fn daemon(&self) -> Command {
        let poll_interval = DAEMON_POLL_INTERVAL.as_millis().to_string();
        self.claudear(
            DAEMON,
            &[
                "start",
                "--foreground",
                "--poll",
                "--poll-interval",
                &poll_interval,
                "--no-webhooks",
                "--no-dashboard",
            ],
        )
    }

    async fn start(&self) -> Child {
        self.launch(self.daemon()).await
    }

    /// Starts the daemon with SIGHUP ignored, as `nohup` starts it.
    async fn start_under_nohup(&self) -> Child {
        let mut command = self.daemon();
        // SAFETY: the hook runs between fork and exec and only calls set_disposition, which is
        // async-signal-safe.
        unsafe { command.pre_exec(|| set_disposition(&[libc::SIGHUP], libc::SIG_IGN)) };
        self.launch(command).await
    }

    /// Starts the daemon the way `claudear start` does by default: forked into a session of its
    /// own, away from this test, which learns its PID from the PID file once it answers IPC.
    async fn start_detached(&self) -> Detached {
        let port = free_port().to_string();
        let started = timeout(
            STARTUP_WAIT,
            self.claudear(DETACHED, &["start", "--port", &port])
                .status(),
        )
        .await
        .unwrap_or_else(|_| {
            panic!(
                "claudear start did not return within {}s\n{}",
                STARTUP_WAIT.as_secs(),
                self.diagnostics()
            )
        })
        .expect("run claudear start");
        assert!(
            started.success(),
            "claudear start failed with {started}\n{}",
            self.diagnostics()
        );
        let deadline = Instant::now() + STARTUP_WAIT;
        loop {
            if UnixStream::connect(self.socket()).await.is_ok() {
                if let Some(pid) = self.daemon_pid() {
                    return Detached { pid };
                }
            }
            assert!(
                Instant::now() < deadline,
                "the detached daemon did not accept IPC connections within {}s\n{}",
                STARTUP_WAIT.as_secs(),
                self.diagnostics()
            );
            sleep(POLL_INTERVAL).await;
        }
    }

    /// Spawns claudear with `arguments` and waits until it answers HTTP on `port`.
    async fn serve(&self, name: &str, arguments: &[&str], port: u16) -> Child {
        let mut claudear = self
            .claudear(name, arguments)
            .spawn()
            .expect("spawn claudear");
        let deadline = Instant::now() + STARTUP_WAIT;
        loop {
            if let Some(status) = claudear.try_wait().expect("check on claudear") {
                panic!(
                    "claudear exited during startup with {status}\n{}",
                    self.diagnostics()
                );
            }
            if serves_http(port).await {
                return claudear;
            }
            assert!(
                Instant::now() < deadline,
                "claudear did not answer HTTP on port {port} within {}s\n{}",
                STARTUP_WAIT.as_secs(),
                self.diagnostics()
            );
            sleep(POLL_INTERVAL).await;
        }
    }

    async fn launch(&self, mut command: Command) -> Child {
        let mut daemon = command.spawn().expect("spawn the daemon");
        self.wait_until_ready(&mut daemon).await;
        daemon
    }

    async fn wait_until_ready(&self, daemon: &mut Child) {
        let deadline = Instant::now() + STARTUP_WAIT;
        loop {
            if let Some(status) = daemon.try_wait().expect("check on the daemon") {
                panic!(
                    "the daemon exited during startup with {status}\n{}",
                    self.diagnostics()
                );
            }
            if UnixStream::connect(self.socket()).await.is_ok() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the daemon did not accept IPC connections within {}s\n{}",
                STARTUP_WAIT.as_secs(),
                self.diagnostics()
            );
            sleep(POLL_INTERVAL).await;
        }
    }

    /// Runs `claudear stop`, which must succeed well before its own timeout: the sandbox's
    /// daemon has no runs to drain.
    async fn stop(&self) {
        let stop = timeout(EXIT_WAIT, self.command(STOP, &["stop"]).status())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "claudear stop did not return within {}s\n{}",
                    EXIT_WAIT.as_secs(),
                    self.diagnostics()
                )
            })
            .expect("run claudear stop");
        assert!(
            stop.success(),
            "claudear stop failed with {stop}\n{}",
            self.diagnostics()
        );
    }

    async fn stop_with(&self, claudear: &mut Child, signal: libc::c_int) -> ExitStatus {
        send(claudear, signal);
        self.wait_for_exit(claudear).await
    }

    async fn wait_for_exit(&self, claudear: &mut Child) -> ExitStatus {
        timeout(EXIT_WAIT, claudear.wait())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "claudear did not exit within {}s\n{}",
                    EXIT_WAIT.as_secs(),
                    self.diagnostics()
                )
            })
            .expect("wait for claudear")
    }

    /// Sends `signals` while the daemon is suspended, so it receives them together and the
    /// drain the first one starts cannot finish before the next one arrives.
    async fn send_together(&self, daemon: &Child, signals: &[libc::c_int]) {
        send(daemon, libc::SIGSTOP);
        self.wait_until_suspended(daemon).await;
        for &signal in signals {
            send(daemon, signal);
        }
        send(daemon, libc::SIGCONT);
    }

    async fn wait_until_suspended(&self, daemon: &Child) {
        let pid = pid_of(daemon);
        let deadline = Instant::now() + SUSPEND_WAIT;
        loop {
            let mut status: libc::c_int = 0;
            // SAFETY: WNOHANG keeps waitpid from blocking, and WUNTRACED reports the stop
            // without reaping the daemon, which the test still owns.
            let changed =
                unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED | libc::WNOHANG) };
            if changed == pid && libc::WIFSTOPPED(status) {
                return;
            }
            assert_eq!(
                changed,
                0,
                "waitpid reported status {status} for the daemon\n{}",
                self.diagnostics()
            );
            assert!(
                Instant::now() < deadline,
                "the daemon was not suspended within {}s\n{}",
                SUSPEND_WAIT.as_secs(),
                self.diagnostics()
            );
            sleep(POLL_INTERVAL).await;
        }
    }

    /// Waits for the stub agent CLI that `claudear` started to report its PID.
    async fn wait_for_agent(&self, claudear: &mut Child) -> Agent {
        let deadline = Instant::now() + STARTUP_WAIT;
        loop {
            if let Ok(pid) = fs::read_to_string(self.agent_pid_file()) {
                return Agent {
                    pid: pid.trim().parse().expect("the agent CLI wrote its PID"),
                };
            }
            if let Some(status) = claudear.try_wait().expect("check on claudear") {
                panic!(
                    "claudear exited with {status} before starting its agent CLI\n{}",
                    self.diagnostics()
                );
            }
            assert!(
                Instant::now() < deadline,
                "claudear did not start its agent CLI within {}s\n{}",
                STARTUP_WAIT.as_secs(),
                self.diagnostics()
            );
            sleep(POLL_INTERVAL).await;
        }
    }

    async fn assert_agent_interrupted(&self) {
        let deadline = Instant::now() + EXIT_WAIT;
        while !self.agent_interrupted_file().exists() {
            assert!(
                Instant::now() < deadline,
                "claudear did not interrupt its agent CLI within {}s\n{}",
                EXIT_WAIT.as_secs(),
                self.diagnostics()
            );
            sleep(POLL_INTERVAL).await;
        }
    }

    async fn assert_running(&self) {
        let status = IpcClient::with_socket_path(self.socket())
            .status()
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "the daemon did not answer a status request: {error}\n{}",
                    self.diagnostics()
                )
            });
        assert!(
            matches!(&status, IpcResponse::Ok(IpcData::State(state)) if state.running),
            "unexpected status {status:?}\n{}",
            self.diagnostics()
        );
    }

    fn runtime_directory(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            // SAFETY: getuid has no preconditions and cannot fail.
            let uid = unsafe { libc::getuid() };
            self.path().join(format!("claudear-{uid}"))
        } else {
            self.path().to_path_buf()
        }
    }

    fn socket(&self) -> PathBuf {
        self.runtime_directory().join("claudear.sock")
    }

    fn pid_file(&self) -> PathBuf {
        self.runtime_directory().join("claudear.pid")
    }

    fn lock_file(&self) -> PathBuf {
        self.runtime_directory().join("claudear.lock")
    }

    /// Takes the daemon lock, as a daemon still starting up holds it before it listens on its
    /// socket.
    fn hold_lock(&self) -> File {
        fs::create_dir_all(self.runtime_directory()).expect("create the runtime directory");
        let lock = File::create(self.lock_file()).expect("create the lock file");
        lock.try_lock().expect("take the daemon lock");
        lock
    }

    /// Leaves a socket file that nothing listens on at the daemon's socket path, as a daemon that
    /// crashed does, and returns its inode.
    fn leave_stale_socket(&self) -> u64 {
        drop(UnixListener::bind(self.socket()).expect("bind the daemon's socket"));
        inode(&self.socket()).expect("the socket file outlives its listener")
    }

    fn daemon_pid(&self) -> Option<libc::pid_t> {
        fs::read_to_string(self.pid_file())
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.output(name)).unwrap_or_default()
    }

    fn assert_files_removed(&self) {
        assert!(
            !self.socket().exists(),
            "the socket file was left behind\n{}",
            self.diagnostics()
        );
        assert!(
            !self.pid_file().exists(),
            "the PID file was left behind\n{}",
            self.diagnostics()
        );
    }

    fn log(&self) -> String {
        self.log_files()
            .map(|path| fs::read_to_string(path).unwrap_or_default())
            .collect()
    }
}

/// A daemon that `claudear start` forked into the background, which is not this test's child,
/// killed if the test fails before it exits.
struct Detached {
    pid: libc::pid_t,
}

impl Detached {
    /// Sends SIGTERM and waits until the daemon has exited.
    async fn terminate(&self, sandbox: &Sandbox) {
        deliver(self.pid, libc::SIGTERM);
        let deadline = Instant::now() + EXIT_WAIT;
        while is_running(self.pid).await {
            assert!(
                Instant::now() < deadline,
                "the detached daemon did not exit within {}s of SIGTERM\n{}",
                EXIT_WAIT.as_secs(),
                sandbox.diagnostics()
            );
            sleep(POLL_INTERVAL).await;
        }
    }
}

impl Drop for Detached {
    fn drop(&mut self) {
        kill_if_alive(self.pid);
    }
}

/// The stub agent CLI a run started, killed if the test fails while it runs.
struct Agent {
    pid: libc::pid_t,
}

impl Drop for Agent {
    fn drop(&mut self) {
        kill_if_alive(self.pid);
    }
}

/// Stands in for Jira: hands every request to the test and leaves it unanswered until the test
/// answers or drops it, so whatever claudear asks Jira waits until then.
struct Jira {
    address: SocketAddr,
    requests: mpsc::UnboundedReceiver<Request>,
}

impl Jira {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the Jira stub");
        let address = listener.local_addr().expect("read the Jira stub's address");
        let (sender, requests) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut head = [0; 1024];
                let length = stream.read(&mut head).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&head[..length]).into_owned();
                if sender.send(Request { head, stream }).is_err() {
                    return;
                }
            }
        });
        Self { address, requests }
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// The next request claudear sends, failing if claudear exits first.
    async fn next_request(&mut self, claudear: &mut Child, sandbox: &Sandbox) -> Request {
        let deadline = Instant::now() + STARTUP_WAIT;
        loop {
            if let Ok(Some(request)) = timeout(POLL_INTERVAL, self.requests.recv()).await {
                return request;
            }
            if let Some(status) = claudear.try_wait().expect("check on claudear") {
                panic!(
                    "claudear exited with {status} before asking Jira anything\n{}",
                    sandbox.diagnostics()
                );
            }
            assert!(
                Instant::now() < deadline,
                "claudear did not ask Jira anything within {}s\n{}",
                STARTUP_WAIT.as_secs(),
                sandbox.diagnostics()
            );
        }
    }
}

/// A request to the [`Jira`] stub, whose connection stays open until it is answered or dropped.
struct Request {
    head: String,
    stream: TcpStream,
}

impl Request {
    async fn answer(mut self, body: &str) {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        self.stream
            .write_all(response.as_bytes())
            .await
            .expect("answer claudear's Jira request");
    }
}

fn inode(path: &Path) -> Option<u64> {
    fs::symlink_metadata(path)
        .ok()
        .map(|metadata| metadata.ino())
}

fn pid_of(daemon: &Child) -> libc::pid_t {
    let id = daemon.id().expect("the daemon has not been reaped");
    libc::pid_t::try_from(id).expect("the PID fits in pid_t")
}

fn is_alive(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only checks that the process exists; nothing is delivered.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Whether `pid` is running. A zombie counts as exited: an orphan is reaped by whoever adopts
/// it, which may never get to it, such as in a container without an init process.
async fn is_running(pid: libc::pid_t) -> bool {
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .await
        .expect("run ps");
    let stat = String::from_utf8_lossy(&output.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

fn kill_if_alive(pid: libc::pid_t) {
    if is_alive(pid) {
        // SAFETY: kill takes no pointers and only sends a signal.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

fn send(claudear: &Child, signal: libc::c_int) {
    deliver(pid_of(claudear), signal);
}

fn deliver(pid: libc::pid_t, signal: libc::c_int) {
    // SAFETY: kill only sends `signal` to a claudear this test started.
    let sent = unsafe { libc::kill(pid, signal) };
    assert_eq!(
        sent,
        0,
        "send signal {signal} to process {pid}: {}",
        io::Error::last_os_error()
    );
}

fn set_disposition(signals: &[libc::c_int], disposition: libc::sighandler_t) -> io::Result<()> {
    for &signal in signals {
        // SAFETY: callers pass SIG_IGN or SIG_DFL, so no handler is installed, and signal is
        // async-signal-safe.
        if unsafe { libc::signal(signal, disposition) } == libc::SIG_ERR {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Whether something answers HTTP on `port`, which claudear does only once its services run, by
/// which point it has installed its signal handlers.
async fn serves_http(port: u16) -> bool {
    let check = async {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        stream
            .write_all(b"GET /api/health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .ok()?;
        let mut response = [0; 5];
        stream.read_exact(&mut response).await.ok()?;
        Some(&response == b"HTTP/")
    };
    matches!(timeout(HTTP_CHECK_WAIT, check).await, Ok(Some(true)))
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("find a free port")
        .port()
}

fn assert_exited_cleanly(status: ExitStatus, sandbox: &Sandbox) {
    assert_eq!(
        status.code(),
        Some(0),
        "claudear exited with {status}\n{}",
        sandbox.diagnostics()
    );
}

#[tokio::test]
async fn daemon_without_an_http_server_keeps_running() {
    let sandbox = Sandbox::new();
    let mut daemon = sandbox.start().await;

    sandbox.assert_running().await;
    assert!(
        daemon.try_wait().expect("check on the daemon").is_none(),
        "the daemon exited after startup\n{}",
        sandbox.diagnostics()
    );
    assert!(
        sandbox.socket().exists() && sandbox.pid_file().exists(),
        "a running daemon keeps its socket and PID file, or the checks that shutdown removes \
         them would pass vacuously\n{}",
        sandbox.diagnostics()
    );
}

#[tokio::test]
async fn stop_returns_once_the_daemon_has_exited() {
    let sandbox = Sandbox::new();
    let mut daemon = sandbox.start().await;
    let pid = pid_of(&daemon);
    let exited = tokio::spawn(async move { daemon.wait().await });

    sandbox.stop().await;

    assert!(
        !is_running(pid).await,
        "claudear stop returned before the daemon exited\n{}",
        sandbox.diagnostics()
    );
    assert!(
        sandbox.read(STOP).contains(STOPPED),
        "claudear stop did not report the exit\n{}",
        sandbox.diagnostics()
    );
    let status = timeout(EXIT_WAIT, exited)
        .await
        .expect("the daemon was reaped")
        .expect("the reaper task finished")
        .expect("wait for the daemon");
    assert_exited_cleanly(status, &sandbox);
    sandbox.assert_files_removed();
}

#[tokio::test]
async fn stop_returns_once_the_daemon_has_exited_before_anything_reaps_it() {
    let sandbox = Sandbox::new();
    let mut daemon = sandbox.start().await;
    let pid = pid_of(&daemon);

    sandbox.stop().await;

    assert!(
        !is_running(pid).await,
        "claudear stop returned before the daemon exited\n{}",
        sandbox.diagnostics()
    );
    assert!(
        sandbox.read(STOP).contains(STOPPED),
        "claudear stop did not report the exit\n{}",
        sandbox.diagnostics()
    );
    let status = sandbox.wait_for_exit(&mut daemon).await;
    assert_exited_cleanly(status, &sandbox);
    sandbox.assert_files_removed();
}

#[tokio::test]
async fn start_refuses_while_another_daemon_holds_the_lock() {
    let sandbox = Sandbox::new();
    let _lock = sandbox.hold_lock();
    let socket = sandbox.leave_stale_socket();

    let mut daemon = sandbox.daemon().spawn().expect("spawn the daemon");
    let status = timeout(STARTUP_WAIT, daemon.wait())
        .await
        .unwrap_or_else(|_| {
            panic!(
                "the daemon was still running {}s after starting while another held the lock\n{}",
                STARTUP_WAIT.as_secs(),
                sandbox.diagnostics()
            )
        })
        .expect("wait for the daemon");

    assert!(
        !status.success(),
        "the daemon started while another held the lock\n{}",
        sandbox.diagnostics()
    );
    assert!(
        sandbox.read(DAEMON).contains(ALREADY_RUNNING),
        "the daemon did not report that another one is running\n{}",
        sandbox.diagnostics()
    );
    assert_eq!(
        inode(&sandbox.socket()),
        Some(socket),
        "the daemon replaced the socket while another held the lock\n{}",
        sandbox.diagnostics()
    );
    assert!(
        !sandbox.pid_file().exists(),
        "the daemon wrote its PID while another held the lock\n{}",
        sandbox.diagnostics()
    );
}

#[tokio::test]
async fn sigterm_drains_and_exits_cleanly() {
    let sandbox = Sandbox::new();
    let mut daemon = sandbox.start().await;

    let status = sandbox.stop_with(&mut daemon, libc::SIGTERM).await;

    assert_exited_cleanly(status, &sandbox);
    sandbox.assert_files_removed();
}

#[tokio::test]
async fn sigint_drains_and_exits_cleanly() {
    let sandbox = Sandbox::new();
    let mut daemon = sandbox.start().await;

    let status = sandbox.stop_with(&mut daemon, libc::SIGINT).await;

    assert_exited_cleanly(status, &sandbox);
    sandbox.assert_files_removed();
}

#[tokio::test]
async fn sighup_drains_and_exits_cleanly() {
    let sandbox = Sandbox::new();
    let mut daemon = sandbox.start().await;

    let status = sandbox.stop_with(&mut daemon, libc::SIGHUP).await;

    assert_exited_cleanly(status, &sandbox);
    sandbox.assert_files_removed();
}

#[tokio::test]
async fn sighup_under_nohup_leaves_the_daemon_running() {
    let sandbox = Sandbox::new();
    let mut daemon = sandbox.start_under_nohup().await;

    send(&daemon, libc::SIGHUP);

    if let Ok(status) = timeout(HANGUP_WAIT, daemon.wait()).await {
        panic!(
            "the daemon exited after an ignored SIGHUP with {status:?}\n{}",
            sandbox.diagnostics()
        );
    }
    sandbox.assert_running().await;
    let status = sandbox.stop_with(&mut daemon, libc::SIGTERM).await;
    assert_exited_cleanly(status, &sandbox);
    sandbox.assert_files_removed();
}

#[tokio::test]
async fn second_signal_forces_the_exit_after_flushing_the_log() {
    let sandbox = Sandbox::new();
    let mut daemon = sandbox.start().await;

    sandbox
        .send_together(&daemon, &[libc::SIGINT, libc::SIGTERM])
        .await;
    let status = sandbox.wait_for_exit(&mut daemon).await;

    assert_eq!(
        status.signal(),
        Some(libc::SIGINT),
        "a forced daemon must die of SIGINT, so a calling shell stops too, but it exited with {status}\n{}",
        sandbox.diagnostics()
    );
    assert!(
        sandbox.log().contains(FORCED_NOTICE),
        "the forced shutdown never reached the log file\n{}",
        sandbox.diagnostics()
    );
    sandbox.assert_files_removed();
}

#[tokio::test]
async fn sigterm_drains_a_daemon_started_in_the_background() {
    let sandbox = Sandbox::new();
    let daemon = sandbox.start_detached().await;

    daemon.terminate(&sandbox).await;

    sandbox.assert_files_removed();
}

#[tokio::test]
async fn sigterm_drains_webhook_mode() {
    let sandbox = Sandbox::new();
    let port = free_port();
    let mut claudear = sandbox
        .serve(WEBHOOK, &["webhook", &port.to_string()], port)
        .await;

    let status = sandbox.stop_with(&mut claudear, libc::SIGTERM).await;

    assert_exited_cleanly(status, &sandbox);
}

#[tokio::test]
async fn sigterm_drains_poll_mode() {
    let sandbox = Sandbox::new();
    let port = free_port();
    let mut claudear = sandbox
        .serve(POLL, &["poll", "--port", &port.to_string()], port)
        .await;

    let status = sandbox.stop_with(&mut claudear, libc::SIGTERM).await;

    assert_exited_cleanly(status, &sandbox);
}

#[tokio::test]
async fn sigterm_interrupts_the_agent_cli_of_a_one_shot_command() {
    let mut jira = Jira::start().await;
    let sandbox = Sandbox::with_jira(&jira.url());
    let mut claudear = sandbox
        .claudear(ACTION, &["action", "reply", "jira", ISSUE_KEY])
        .spawn()
        .expect("spawn claudear action");
    let request = jira.next_request(&mut claudear, &sandbox).await;
    assert!(
        request.head.contains(ISSUE_KEY),
        "claudear asked Jira for something other than {ISSUE_KEY} first:\n{}",
        request.head
    );
    request.answer(ISSUE).await;
    drop(jira);
    let _agent = sandbox.wait_for_agent(&mut claudear).await;

    send(&claudear, libc::SIGTERM);

    sandbox.assert_agent_interrupted().await;
    let status = sandbox.wait_for_exit(&mut claudear).await;
    assert_eq!(
        status.signal(),
        Some(libc::SIGINT),
        "an interrupted one-shot command must exit as interrupted, dying of SIGINT, but it \
         exited with {status}\n{}",
        sandbox.diagnostics()
    );
}

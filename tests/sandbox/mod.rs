use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tempfile::TempDir;
use tokio::process::Command;

const AGENT: &str = "claude";
const AGENT_INTERRUPTED: &str = "agent.interrupted";
const AGENT_PID: &str = "agent.pid";
const CONFIG: &str = "claudear.toml";
const DATABASE: &str = "claudear.db";
const LOGS: &str = "logs";
const OUTPUT_EXTENSION: &str = "out";
const TAIL_LINES: usize = 40;
const UNREACHABLE_URL: &str = "http://127.0.0.1:1";
const WORKSPACE: &str = "workspace";

/// A home, config and database for the claudear binary, whose only source is a Jira and whose
/// retries are due at once. Its agent CLI is a stub that runs until a SIGINT, which it records.
/// It lives under /tmp to keep the daemon's socket path short.
pub struct Sandbox {
    root: TempDir,
}

impl Sandbox {
    /// A sandbox whose Jira nothing listens for.
    pub fn new() -> Self {
        Self::with_jira(UNREACHABLE_URL)
    }

    /// A sandbox whose Jira is at `jira_url`.
    pub fn with_jira(jira_url: &str) -> Self {
        let root = tempfile::Builder::new()
            .prefix("claudear")
            .tempdir_in("/tmp")
            .expect("create a sandbox under /tmp");
        let sandbox = Self { root };
        fs::create_dir(sandbox.path().join(WORKSPACE)).expect("create the sandbox workspace");
        sandbox.install_agent();
        fs::write(
            sandbox.path().join(CONFIG),
            config(sandbox.path(), &sandbox.database(), jira_url),
        )
        .expect("write the sandbox config");
        sandbox
    }

    pub fn path(&self) -> &Path {
        self.root.path()
    }

    pub fn database(&self) -> PathBuf {
        self.path().join(DATABASE)
    }

    /// Where the stub agent CLI writes its PID once it handles SIGINT.
    pub fn agent_pid_file(&self) -> PathBuf {
        self.path().join(AGENT_PID)
    }

    /// Where the stub agent CLI records the SIGINT that ends it.
    pub fn agent_interrupted_file(&self) -> PathBuf {
        self.path().join(AGENT_INTERRUPTED)
    }

    /// Claudear with `arguments`, run inside the sandbox with its stdout and stderr going to the
    /// output file for `name`.
    pub fn command(&self, name: &str, arguments: &[&str]) -> Command {
        let output = File::create(self.output(name)).expect("create the output file");
        let mut command = Command::new(env!("CARGO_BIN_EXE_claudear"));
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.path())
            .env("TMPDIR", self.path())
            .env("XDG_RUNTIME_DIR", self.path())
            .current_dir(self.path())
            .arg("--config")
            .arg(self.path().join(CONFIG))
            .arg("--log-dir")
            .arg(self.path().join(LOGS))
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(output.try_clone().expect("share the output file"))
            .stderr(output)
            .kill_on_drop(true);
        command
    }

    pub fn output(&self, name: &str) -> PathBuf {
        self.path().join(format!("{name}.{OUTPUT_EXTENSION}"))
    }

    pub fn log_files(&self) -> impl Iterator<Item = PathBuf> {
        files_in(&self.path().join(LOGS))
    }

    /// The tail of every command's output and every log file, for failure messages.
    pub fn diagnostics(&self) -> String {
        let mut outputs: Vec<PathBuf> = files_in(self.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == OUTPUT_EXTENSION)
            })
            .collect();
        outputs.sort();
        outputs
            .into_iter()
            .chain(self.log_files())
            .map(|path| format!("{}:\n{}", path.display(), tail(&path)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Installs an agent CLI that reads its prompt, reports its PID once it handles SIGINT, then
    /// runs until a SIGINT, which it records. Perl, because a shell cannot trap a signal it
    /// inherited as ignored.
    fn install_agent(&self) {
        let script = format!(
            r#"#!/bin/sh
cat > /dev/null
exec perl -e '$SIG{{INT}} = sub {{ open(my $f, ">", "{interrupted}") or die; close $f; exit 130 }}; open(my $f, ">", "{started}.partial") or die; print $f "$$\n"; close $f; rename("{started}.partial", "{started}") or die; sleep 1 while 1'
"#,
            interrupted = self.agent_interrupted_file().display(),
            started = self.agent_pid_file().display(),
        );
        let agent = self.path().join(AGENT);
        fs::write(&agent, script).expect("write the stub agent CLI");
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o755))
            .expect("make the stub agent CLI executable");
    }
}

fn config(root: &Path, database: &Path, jira_url: &str) -> String {
    let root = root.display();
    let database = database.display();
    format!(
        r#"workspace = "{root}/{WORKSPACE}"
db_path = "{database}"
storage_dir = "{root}/storage"
known_orgs = []
auto_discover_paths = []

[code_index]
enabled = false

[regression]
enabled = false

[retry]
base_delay_ms = 0
max_delay_ms = 0

[agent.providers.claude]
binary = "{root}/{AGENT}"

[issues.jira]
enabled = true
base_url = "{jira_url}"
email = "sandbox@example.com"
api_token = "unused"
project_keys = ["SANDBOX"]
"#
    )
}

fn files_in(directory: &Path) -> impl Iterator<Item = PathBuf> {
    fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
}

fn tail(path: &Path) -> String {
    let content = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = content.lines().collect();
    lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n")
}

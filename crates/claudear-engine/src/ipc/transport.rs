//! Platform-abstracted IPC transport.
//!
//! On Unix, uses Unix domain sockets.  On Windows, uses TCP on localhost with
//! a port file for daemon discovery.
//!
//! Consumers import [`IpcListener`], [`IpcStream`], [`connect`], [`bind`], and
//! [`check_connection`] without any `#[cfg]` in their own code.
//!
//! A Unix socket in a mode-0700 directory is already restricted to its owner.
//! A loopback TCP port is not: every local process can reach it. So on Windows
//! the port file also carries a secret that the listener generates at bind
//! time, and a connection must present it before the server reads a command.
//! The file lives in `%LOCALAPPDATA%`, which is per-user, so possession of the
//! secret is the same boundary the socket mode gives on Unix.

use std::io;
use std::path::Path;

// ---------------------------------------------------------------------------
// Type aliases — one concrete type per platform, transparent to callers
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
pub type IpcListener = tokio::net::UnixListener;
#[cfg(windows)]
pub type IpcListener = tokio::net::TcpListener;

#[cfg(not(windows))]
pub type IpcStream = tokio::net::UnixStream;
#[cfg(windows)]
pub type IpcStream = tokio::net::TcpStream;

#[cfg(windows)]
static SECRET: std::sync::OnceLock<String> = std::sync::OnceLock::new();

// ---------------------------------------------------------------------------
// Connection helpers
// ---------------------------------------------------------------------------

/// Connect to an IPC endpoint at `path`.
///
/// * **Unix** — connects to the Unix domain socket at `path`.
/// * **Windows** — reads a TCP port number from `path` and connects to
///   `127.0.0.1:<port>`.
pub async fn connect(path: &Path) -> io::Result<IpcStream> {
    #[cfg(not(windows))]
    {
        IpcStream::connect(path).await
    }
    #[cfg(windows)]
    {
        let (port, _) = read_endpoint(path)?;
        IpcStream::connect(("127.0.0.1", port)).await
    }
}

/// Bind an IPC listener at `path`.
///
/// * **Unix** — binds a Unix domain socket at `path`.
/// * **Windows** — binds TCP on `127.0.0.1:0` (ephemeral port) and writes the
///   assigned port and a freshly generated secret to `path`.
pub fn bind(path: &Path) -> io::Result<IpcListener> {
    #[cfg(not(windows))]
    {
        IpcListener::bind(path)
    }
    #[cfg(windows)]
    {
        // Use std to bind synchronously so we get the port immediately.
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = std_listener.local_addr()?.port();
        let secret = SECRET.get_or_init(generate_secret);
        write_endpoint(path, port, secret)?;
        std_listener.set_nonblocking(true)?;
        IpcListener::from_std(std_listener)
    }
}

/// The secret a connection must present, if this transport authenticates.
///
/// `None` on Unix, where the socket's directory mode is the access boundary.
pub fn expected_secret() -> Option<&'static str> {
    #[cfg(not(windows))]
    {
        None
    }
    #[cfg(windows)]
    {
        SECRET.get().map(String::as_str)
    }
}

/// The secret to present when connecting to the endpoint at `path`.
///
/// `None` on Unix.
pub fn client_secret(path: &Path) -> io::Result<Option<String>> {
    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(None)
    }
    #[cfg(windows)]
    {
        read_endpoint(path).map(|(_, secret)| Some(secret))
    }
}

/// Compare two secrets without leaking their contents through timing.
pub fn secret_matches(presented: &str, expected: &str) -> bool {
    let presented = presented.as_bytes();
    let expected = expected.as_bytes();
    if presented.len() != expected.len() {
        return false;
    }

    let mut difference = 0u8;
    for (a, b) in presented.iter().zip(expected) {
        difference |= a ^ b;
    }
    difference == 0
}

/// Synchronously check whether a daemon is reachable at `path`.
///
/// Returns `true` if a connection can be established.
pub fn check_connection(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    #[cfg(not(windows))]
    {
        std::os::unix::net::UnixStream::connect(path).is_ok()
    }
    #[cfg(windows)]
    {
        read_endpoint(path)
            .map(|(port, _)| std::net::TcpStream::connect(("127.0.0.1", port)).is_ok())
            .unwrap_or(false)
    }
}

/// Check whether the IPC endpoint at `path` is stale (file exists but nobody
/// is listening).
pub fn is_stale(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    #[cfg(not(windows))]
    {
        std::os::unix::net::UnixStream::connect(path).is_err()
    }
    #[cfg(windows)]
    {
        read_endpoint(path)
            .map(|(port, _)| std::net::TcpStream::connect(("127.0.0.1", port)).is_err())
            .unwrap_or(true)
    }
}

// ---------------------------------------------------------------------------
// Port file helpers (Windows only)
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn generate_secret() -> String {
    hex::encode(rand::random::<[u8; 32]>())
}

/// Read the `<port>\n<secret>` pair written by [`bind`].
#[cfg(windows)]
pub(crate) fn read_endpoint(path: &Path) -> io::Result<(u16, String)> {
    let contents = std::fs::read_to_string(path)?;
    let mut lines = contents.lines();

    let port: u16 = lines
        .next()
        .unwrap_or_default()
        .trim()
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let secret = lines.next().unwrap_or_default().trim().to_string();
    if secret.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "port file carries no secret",
        ));
    }

    Ok((port, secret))
}

#[cfg(windows)]
fn write_endpoint(path: &Path, port: u16, secret: &str) -> io::Result<()> {
    std::fs::write(path, format!("{}\n{}\n", port, secret))
}

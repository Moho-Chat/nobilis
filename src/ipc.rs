//! Where a frontend and the daemon meet, on whichever system this is.
//!
//! nobilis talks newline-delimited JSON to local clients over a local IPC
//! primitive. Which primitive that is, is the only genuinely
//! operating-system-shaped decision in the daemon, so it lives here and
//! nowhere else: everything above this module - all ninety-odd RPC methods,
//! the event fan-out, the per-connection subscriptions - is written against
//! `Conn` and never learns what it actually is.
//!
//! Deliberately not a TCP socket on loopback, which would have been one
//! implementation for both. A loopback port is reachable by every process and
//! every user on the machine, so it would have to carry a token to be safe at
//! all, and the token would have to be stored somewhere with the very
//! filesystem permissions this arrangement is trying to rely on. Both systems
//! offer a local primitive that answers "who may connect" with the operating
//! system's own access control, so both use theirs:
//!
//!   - Unix domain socket in a 0700 directory, on Linux and macOS
//!   - Named pipe with a default DACL, on Windows
//!
//! A named pipe is not a file, so on Windows the "path" is a pipe name.
//! `PathBuf` carries both because a pipe name is a perfectly good path string,
//! and using one type keeps the `--socket-path` option meaningful everywhere.

use anyhow::Result;
use std::path::{Path, PathBuf};

#[cfg(unix)]
mod imp {
    use anyhow::{Context, Result};
    use std::path::{Path, PathBuf};

    /// One client's connection.
    pub type Conn = tokio::net::UnixStream;

    pub struct Listener {
        inner: tokio::net::UnixListener,
    }

    impl Listener {
        pub async fn bind(path: &Path) -> Result<Self> {
            if let Some(dir) = path.parent() {
                tokio::fs::create_dir_all(dir)
                    .await
                    .with_context(|| format!("creating {}", dir.display()))?;
                // The directory is the access control: 0700 means only this
                // user can reach the socket inside it, whatever the socket's
                // own mode ends up being.
                crate::secure::restrict_to_owner(dir)?;
            }
            // A socket file left behind by a crash would refuse the bind. The
            // singleton lock in main.rs has already established that no other
            // instance is running, so anything still here is stale.
            if path.exists() {
                tokio::fs::remove_file(path).await.ok();
            }
            let inner = tokio::net::UnixListener::bind(path)
                .with_context(|| format!("binding {}", path.display()))?;
            Ok(Self { inner })
        }

        pub async fn accept(&mut self) -> Result<Conn> {
            let (stream, _addr) = self.inner.accept().await?;
            Ok(stream)
        }
    }

    /// The socket this system puts under the user's runtime directory.
    pub fn default_endpoint() -> PathBuf {
        // XDG_RUNTIME_DIR is a per-user directory the system already
        // guarantees is private and cleaned up at logout, which is exactly
        // what a socket wants. /tmp is the fallback for a session that has
        // none, where the 0700 directory above does the same job by hand.
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::runtime_dir())
            .unwrap_or_else(std::env::temp_dir)
            .join("nobilis")
            .join("nobilis.sock")
    }
}

#[cfg(windows)]
mod imp {
    use anyhow::{Context, Result};
    use std::path::{Path, PathBuf};
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    /// One client's connection.
    pub type Conn = NamedPipeServer;

    /// A named pipe listener.
    ///
    /// Windows models this differently from a Unix socket in a way that
    /// cannot be hidden any further down: a pipe *instance* serves exactly one
    /// client, so accepting means handing over the instance that was waiting
    /// and immediately creating the next one. Getting that order wrong leaves
    /// a window with no instance listening, during which a connecting client
    /// is refused outright rather than queued - so the replacement is created
    /// before the connected instance is handed out.
    pub struct Listener {
        name: String,
        next: Option<NamedPipeServer>,
    }

    impl Listener {
        pub async fn bind(path: &Path) -> Result<Self> {
            let name = path.to_string_lossy().into_owned();
            // first_pipe_instance refuses if another process already owns this
            // name, which is the same protection the Unix side gets from the
            // singleton lock plus an exclusive bind.
            let first = ServerOptions::new()
                .first_pipe_instance(true)
                .create(&name)
                .with_context(|| format!("creating named pipe {name}"))?;
            Ok(Self { name, next: Some(first) })
        }

        pub async fn accept(&mut self) -> Result<Conn> {
            let server = match self.next.take() {
                Some(s) => s,
                None => ServerOptions::new().create(&self.name)?,
            };
            server.connect().await.context("waiting for a client on the named pipe")?;
            self.next = Some(
                ServerOptions::new()
                    .create(&self.name)
                    .with_context(|| format!("creating the next instance of {}", self.name))?,
            );
            Ok(server)
        }
    }

    /// The pipe name this daemon answers on.
    ///
    /// A pipe has no directory to make private, and needs none: the default
    /// DACL on a pipe created by an ordinary process grants access to its
    /// creator's token and denies everyone else, which is the same answer the
    /// 0700 directory gives on Unix.
    pub fn default_endpoint() -> PathBuf {
        PathBuf::from(r"\\.\pipe\nobilis")
    }
}

pub use imp::{Conn, Listener};

/// Where a frontend should look for this daemon by default.
pub fn default_endpoint() -> PathBuf {
    imp::default_endpoint()
}

/// Opens the local endpoint clients connect to.
pub async fn listen(path: &Path) -> Result<Listener> {
    Listener::bind(path).await
}

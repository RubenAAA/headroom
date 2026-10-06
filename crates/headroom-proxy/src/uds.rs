//! Unix-domain-socket transport for the proxy.
//!
//! Port of upstream `headroom/proxy/uds.py` (commit `e7b3baf2`).
//! `headroom-proxy --uds PATH` serves the same app over an `AF_UNIX` socket
//! instead of a TCP port. Nothing about request handling changes; this is
//! the transport shell. When `--uds` is set the TCP listen address is
//! ignored entirely (upstream arbitrates the same way: the socket wins,
//! `--host`/`--port` are not bound).
//!
//! One platform fact shapes the port: axum serves a `UnixListener` with a
//! `tokio::net::unix::SocketAddr` peer, but every handler extracts
//! `ConnectInfo<std::net::SocketAddr>`, and the orphan rule forbids bridging
//! those two. So `uds_router` serves the same `build_app` plus a layer that
//! attributes every connection to 127.0.0.1. A socket peer is local by
//! construction, and `restrict_socket_mode` makes the socket owner-only,
//! so loopback-scoped guards (metrics gating, operator gates) treat it
//! exactly like a loopback TCP client. All socket clients share
//! that one identity for rate-limit purposes; the directory holding the
//! socket file is the access-control boundary, per upstream.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::ConnectInfo;
use axum::middleware::{self, Next};
use axum::response::Response;

use crate::{AppState, build_app};

#[cfg(unix)]
use std::os::unix::fs::FileTypeExt as _;

/// Docs page carrying the client-compatibility detail the banner has no room for.
pub const UDS_DOCS_URL: &str =
    "https://headroom-docs.vercel.app/docs/proxy#serving-on-a-unix-socket";

/// Peer address attributed to every connection arriving over the socket.
/// Chosen so loopback-scoped guards treat socket clients like loopback TCP
/// clients; a socket peer is local by construction.
pub const UDS_LOOPBACK_PEER: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);

/// A Unix socket path cannot be used for the reason described.
#[derive(Debug, thiserror::Error)]
pub enum UdsError {
    /// The path (or its parents) fails a preflight check.
    #[error("{0}")]
    Invalid(String),
    /// An unexpected filesystem error during preflight.
    #[error("socket preflight I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Longest usable socket path, including the trailing NUL: 108 on Linux,
/// 104 on the BSDs (upstream takes a platform parameter for tests; here it
/// is a `cfg`, which is the same fact).
pub fn max_uds_path_length() -> usize {
    #[cfg(target_os = "linux")]
    {
        108
    }
    #[cfg(not(target_os = "linux"))]
    {
        104
    }
}

/// True when something is already accepting connections on `path`.
///
/// A leftover socket file from a crashed proxy looks identical to a live one
/// on disk; the only way to tell them apart is to try connecting.
async fn is_live_socket(path: &Path) -> bool {
    match tokio::time::timeout(
        std::time::Duration::from_millis(500),
        tokio::net::UnixStream::connect(path),
    )
    .await
    {
        // ECONNREFUSED: nothing is listening, the inode is stale.
        // ENOENT: it vanished between the stat and the connect.
        Ok(Err(e))
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            false
        }
        // Connected, or EACCES/ETIMEDOUT/anything else: something is there,
        // or we cannot tell. Either way, refuse to unlink it.
        _ => true,
    }
}

/// How the startup banner describes a socket bind.
///
/// Deliberately names no agent and hands out no environment variables (port
/// of the `socket_usage_lines` rule: the banner states a transport
/// requirement and points at the docs; per-client wiring belongs in the
/// docs, where it can be qualified).
pub fn socket_usage_lines(path: &Path) -> [String; 4] {
    [
        format!("  Socket:        {}", path.display()),
        "  Client:        must support HTTP over a Unix socket natively".to_string(),
        format!(
            "  Example:       curl --unix-socket {} http://localhost/health",
            path.display()
        ),
        format!("  Details:       {UDS_DOCS_URL}"),
    ]
}

/// Directories along `target` that do not exist yet, shallowest first.
fn missing_ancestors(target: &Path) -> Vec<PathBuf> {
    let mut missing = Vec::new();
    let mut node = target.to_path_buf();
    while !node.exists() {
        missing.push(node.clone());
        match node.parent() {
            Some(parent) if parent != node => node = parent.to_path_buf(),
            // Reached the filesystem root (or a prefix-less relative stub).
            _ => break,
        }
    }
    missing.reverse();
    missing
}

/// Reject a pre-existing parent that lets other users swap the socket.
///
/// Deliberately does not repair the mode. The directory predates this call,
/// so changing it could lock out whatever put it there.
///
/// Group/world-writable is tolerated when the sticky bit is set, which is the
/// `/tmp` case: others may create their own entries but cannot unlink or
/// rename ours, so the socket cannot be swapped out from under us.
fn require_safe_existing_parent(parent: &Path) -> Result<(), UdsError> {
    use std::os::unix::fs::MetadataExt as _;

    let mode = match std::fs::metadata(parent) {
        Ok(meta) => meta.mode(),
        // Unreadable; bind() will produce the authoritative error.
        Err(_) => return Ok(()),
    };
    if mode & 0o022 == 0 || mode & 0o1000 != 0 {
        return Ok(());
    }
    Err(UdsError::Invalid(format!(
        "refusing to serve a socket from {}: the directory is writable by other \
         users without the sticky bit, so the socket could be swapped out from \
         under the proxy",
        parent.display()
    )))
}

/// Create `parent` (and any missing ancestors) owner-only. Only directories
/// this call creates get `0o700`; pre-existing ancestors keep their mode.
fn prepare_parent_dir(parent: &Path) -> Result<(), UdsError> {
    let missing = missing_ancestors(parent);
    if missing.is_empty() {
        return require_safe_existing_parent(parent);
    }
    std::fs::create_dir_all(parent)?;
    use std::os::unix::fs::PermissionsExt as _;
    for dir in &missing {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Expand `~`, make absolute, and run every preflight check, returning the
/// path to bind. Mirrors upstream `prepare_uds_path` step for step:
///
/// - enforce the `sun_path` byte limit (absolute path plus trailing NUL);
/// - create missing parents owner-only, vet pre-existing ones;
/// - clear a stale socket file, refuse a live one, and never touch any
///   other file type (not even a symlink — it could point anywhere).
///
/// The parent is vetted before anything is unlinked, so a path in an unsafe
/// directory is refused without side effects. (The module is `cfg(unix)`;
/// `main.rs` refuses `--uds` on other platforms.)
pub async fn prepare_uds_path(path: &Path) -> Result<PathBuf, UdsError> {
    use std::os::unix::ffi::OsStrExt as _;

    let resolved = absolute_socket_path(path);
    if resolved.as_os_str().as_bytes().len() + 1 > max_uds_path_length() {
        return Err(UdsError::Invalid(format!(
            "socket path is too long for sun_path ({} > {}): {}",
            resolved.as_os_str().as_bytes().len() + 1,
            max_uds_path_length(),
            resolved.display()
        )));
    }
    if let Some(parent) = resolved.parent() {
        prepare_parent_dir(parent)?;
    }
    match std::fs::symlink_metadata(&resolved) {
        Ok(meta) if meta.file_type().is_socket() => {
            if is_live_socket(&resolved).await {
                return Err(UdsError::Invalid(format!(
                    "socket path is already served by a live proxy: {}",
                    resolved.display()
                )));
            }
            std::fs::remove_file(&resolved)?;
        }
        // Never delete anything that is not a socket: a regular file, a
        // directory, or a symlink (which could point anywhere).
        Ok(_) => {
            return Err(UdsError::Invalid(format!(
                "refusing to replace non-socket file with a socket: {}",
                resolved.display()
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(UdsError::Io(e)),
    }
    Ok(resolved)
}

/// Make the bound socket owner-only (`0600`). Connecting needs write access
/// to the socket file, so this is what limits clients to the proxy's own
/// user whatever the umask or a sticky shared parent (`/tmp`) allows. It is
/// also what justifies [`UDS_LOOPBACK_PEER`]: every peer that can connect
/// runs as the owner, a narrower set than can reach a loopback TCP port.
pub fn restrict_socket_mode(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Remove the socket file on shutdown. Only ever unlinks a socket, so a
/// racing `remove_uds_path` cannot take out anything else.
pub fn remove_uds_path(path: &Path) {
    if std::fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_socket())
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(path);
    }
}

/// Expand a leading `~` against `$HOME`, then make the path absolute
/// against the current directory. Lexical only (no symlink resolution):
/// `bind` follows whatever is there anyway.
fn absolute_socket_path(path: &Path) -> PathBuf {
    let expanded = match path.strip_prefix("~") {
        Ok(rest) => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("~"))
            .join(rest),
        Err(_) => path.to_path_buf(),
    };
    if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir().unwrap_or_default().join(expanded)
    }
}

/// The same app as a TCP bind, plus a layer attributing every connection to
/// the loopback peer (see the module docs for why the peer is synthesized).
pub fn uds_router(state: AppState) -> Router {
    build_app(state).layer(middleware::from_fn(insert_loopback_peer))
}

/// Axum's `UnixListener` serve path reports a unix `SocketAddr` peer, which
/// no handler extracts; insert the loopback `ConnectInfo` instead so
/// request handling is byte-identical to a loopback TCP bind.
async fn insert_loopback_peer(mut req: axum::extract::Request, next: Next) -> Response {
    req.extensions_mut().insert(ConnectInfo(UDS_LOOPBACK_PEER));
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sun_path_limit_matches_platform() {
        #[cfg(target_os = "linux")]
        assert_eq!(max_uds_path_length(), 108);
        #[cfg(not(target_os = "linux"))]
        assert_eq!(max_uds_path_length(), 104);
    }

    #[test]
    fn usage_lines_state_transport_not_agent() {
        let lines = socket_usage_lines(Path::new("/run/headroom/proxy.sock"));
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains("/run/headroom/proxy.sock"));
        assert!(lines[1].contains("Unix socket"));
        assert!(lines[3].contains(UDS_DOCS_URL));
        // Regression rule from upstream: the banner names no agent and hands
        // out no environment variables.
        let joined = lines.join("\n").to_lowercase();
        for token in [
            "claude",
            "codex",
            "gemini",
            "api key",
            "export ",
            "anthropic_",
        ] {
            assert!(!joined.contains(token), "banner leaks {token}");
        }
    }

    #[test]
    fn missing_ancestors_is_empty_for_an_existing_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(missing_ancestors(dir.path()).is_empty());
    }

    #[test]
    fn missing_ancestors_lists_only_absent_levels_shallowest_first() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a").join("b").join("proxy.sock");
        assert_eq!(
            missing_ancestors(&target),
            vec![dir.path().join("a"), dir.path().join("a").join("b"), target]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prepare_creates_parent_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("new-parent").join("proxy.sock");
        let prepared = prepare_uds_path(&sock).await.unwrap();
        assert_eq!(prepared, sock);
        let mode = std::fs::metadata(dir.path().join("new-parent"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prepare_preserves_an_existing_parents_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let sock = dir.path().join("proxy.sock");
        prepare_uds_path(&sock).await.unwrap();
        let mode = std::fs::metadata(dir.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prepare_refuses_a_world_writable_existing_parent() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        let err = prepare_uds_path(&dir.path().join("proxy.sock"))
            .await
            .unwrap_err();
        assert!(matches!(err, UdsError::Invalid(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prepare_accepts_a_sticky_world_writable_parent() {
        let dir = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o1777)).unwrap();
        prepare_uds_path(&dir.path().join("proxy.sock"))
            .await
            .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prepare_refuses_a_live_socket_but_clears_a_stale_one() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("proxy.sock");

        let live = tokio::net::UnixListener::bind(&sock).unwrap();
        let err = prepare_uds_path(&sock).await.unwrap_err();
        assert!(matches!(err, UdsError::Invalid(_)));
        drop(live);

        // Listener closed: the file is stale now, so preflight clears it.
        let prepared = prepare_uds_path(&sock).await.unwrap();
        assert_eq!(prepared, sock);
        assert!(!sock.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prepare_never_replaces_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("proxy.sock");
        std::fs::write(&victim, "not a socket").unwrap();
        let err = prepare_uds_path(&victim).await.unwrap_err();
        assert!(matches!(err, UdsError::Invalid(_)));
        assert!(victim.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prepare_rejects_an_overlong_path() {
        let dir = tempfile::tempdir().unwrap();
        let long = "x".repeat(max_uds_path_length());
        let err = prepare_uds_path(&dir.path().join(long)).await.unwrap_err();
        assert!(matches!(err, UdsError::Invalid(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn remove_only_unlinks_sockets() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("proxy.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        remove_uds_path(&sock);
        assert!(!sock.exists());
        drop(listener);

        let regular = dir.path().join("keep.txt");
        std::fs::write(&regular, "data").unwrap();
        remove_uds_path(&regular);
        assert!(regular.exists());
    }
}

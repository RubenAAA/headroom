//! One relay lane over Proton VPN's free plan, through `wireproxy`.
//!
//! Proton's free plan has no SOCKS5 service and allows one VPN connection per
//! account, so it adds exactly one lane, not a pool. `wireproxy` runs the
//! WireGuard session in userspace and serves it as a loopback SOCKS5 port: no
//! TUN device, no root, no route change, so only traffic sent to that port goes
//! through Proton. (tun2socks does the reverse — SOCKS into a TUN device — and
//! would not help here.)
//!
//! The configs are the WireGuard files Proton's dashboard hands out, one per
//! free server, kept in a private directory. Rotation stops the running tunnel
//! before starting the next config, so two sessions never overlap and the
//! one-connection limit holds. Each free server exits from its own address
//! (measured 2026-09-30: five configs, five distinct exits).

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::{expand_home, home_dir, is_private, process_runs, signal_process};

/// How long a fresh `wireproxy` gets to open its SOCKS port.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

pub fn configs_dir() -> PathBuf {
    std::env::var_os("HEADROOM_PROTON_WG_DIR")
        .map(PathBuf::from)
        .map(expand_home)
        .unwrap_or_else(|| home_dir().join(".config/headroom/proton-wg"))
}

/// `HEADROOM_WIREPROXY_BIN`, else the copy `install.sh` puts in
/// `~/.local/bin`, else the first one on `PATH`.
pub fn wireproxy_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("HEADROOM_WIREPROXY_BIN") {
        return Some(expand_home(PathBuf::from(path)));
    }
    let local = home_dir().join(".local/bin/wireproxy");
    if local.is_file() {
        return Some(local);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join("wireproxy"))
        .find(|path| path.is_file())
}

fn owned_and_private(path: &Path, metadata: &fs::Metadata) -> Result<(), String> {
    if !is_private(metadata) {
        return Err(format!(
            "{} must be owned by this user and private (mode 0700 for the directory, 0600 for configs)",
            path.display()
        ));
    }
    Ok(())
}

/// The `*.conf` files in `dir`, sorted by name. A missing directory means
/// Proton is not set up and yields none; an unsafe one is an error, because
/// every file in it holds a WireGuard private key.
pub fn find_configs(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let metadata = match fs::metadata(dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err("could not inspect the Proton WireGuard directory".to_string()),
    };
    owned_and_private(dir, &metadata)?;
    let mut configs = Vec::new();
    for entry in fs::read_dir(dir).map_err(|_| "could not read the Proton WireGuard directory")? {
        let path = entry
            .map_err(|_| "could not read the Proton WireGuard directory")?
            .path();
        if path.extension().is_some_and(|ext| ext == "conf") {
            let metadata =
                fs::metadata(&path).map_err(|_| "could not inspect a Proton WireGuard config")?;
            owned_and_private(&path, &metadata)?;
            configs.push(path);
        }
    }
    configs.sort();
    Ok(configs)
}

fn free_loopback_port() -> Result<u16, String> {
    TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .map_err(|_| "could not reserve a loopback port for wireproxy".to_string())
}

/// The running `wireproxy` and the configs it rotates through. The SOCKS port
/// is fixed for the daemon's life, so the relay lane in front of it never
/// changes its upstream.
pub struct Tunnel {
    wireproxy: PathBuf,
    configs: Vec<PathBuf>,
    state_dir: PathBuf,
    port: u16,
    current: usize,
    child: Option<Child>,
}

impl Tunnel {
    pub fn new(
        wireproxy: PathBuf,
        configs: Vec<PathBuf>,
        state_dir: PathBuf,
    ) -> Result<Self, String> {
        let tunnel = Self {
            wireproxy,
            configs,
            state_dir,
            port: free_loopback_port()?,
            current: 0,
            child: None,
        };
        tunnel.stop_orphan();
        Ok(tunnel)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn config_count(&self) -> usize {
        self.configs.len()
    }

    pub fn current(&self) -> usize {
        self.current
    }

    /// What status and logs call the lane's upstream: the config's file stem,
    /// which Proton names after the server (`NL-FREE-119`).
    pub fn name(&self) -> String {
        let stem = self.configs[self.current]
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        format!("proton:{stem}")
    }

    /// Config indices in ring order after the current one, the current one last.
    pub fn ring(&self) -> Vec<usize> {
        let n = self.configs.len();
        (1..=n).map(|step| (self.current + step) % n).collect()
    }

    fn pid_path(&self) -> PathBuf {
        self.state_dir.join("wireproxy.pid")
    }

    /// A daemon that died without cleaning up leaves its `wireproxy` running,
    /// holding the account's one connection. Stop it before starting another.
    fn stop_orphan(&self) {
        let Some(pid) = fs::read_to_string(self.pid_path())
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
        else {
            return;
        };
        if process_runs(pid, &["wireproxy"]) {
            let _ = signal_process(pid, libc::SIGTERM);
            eprintln!("proton: stopped an orphaned wireproxy left by an earlier relay");
        }
        let _ = fs::remove_file(self.pid_path());
    }

    /// Stop whatever runs now, then bring up config `index` and wait for its
    /// SOCKS port. The caller probes the exit.
    pub fn start(&mut self, index: usize) -> Result<(), String> {
        self.stop();
        self.current = index;
        let settings = self.state_dir.join("wireproxy.conf");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&settings)
            .map_err(|_| "could not write the wireproxy settings file")?;
        write!(
            file,
            "WGConfig = {}\n\n[Socks5]\nBindAddress = 127.0.0.1:{}\n",
            self.configs[index].display(),
            self.port
        )
        .map_err(|_| "could not write the wireproxy settings file")?;
        drop(file);
        // `-s`: wireproxy logs every destination it resolves, and this relay
        // never logs destinations.
        let mut child = Command::new(&self.wireproxy)
            .arg("-s")
            .arg("-c")
            .arg(&settings)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "could not launch wireproxy")?;
        let _ = fs::write(self.pid_path(), format!("{}\n", child.id()));
        let address = SocketAddr::from(([127, 0, 0, 1], self.port));
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if TcpStream::connect_timeout(&address, Duration::from_millis(250)).is_ok() {
                self.child = Some(child);
                return Ok(());
            }
            if child.try_wait().ok().flatten().is_some() || Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_file(self.pid_path());
                return Err(format!("wireproxy did not come up on {}", self.name()));
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(self.pid_path());
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.stop();
    }
}

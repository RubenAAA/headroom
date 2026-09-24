//! Per-lane local SOCKS5 relays over Nord's authenticated SOCKS5 endpoints.
//!
//! This is an operator sidecar, not part of Headroom's HTTP request path. Its
//! CLI and line-delimited Unix-socket control protocol intentionally remain
//! compatible with the previous helper so the watcher can rotate either
//! implementation during a drained migration.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use reqwest::blocking::{Client, Response};
use reqwest::Proxy;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::signal::unix::SignalKind;
use url::Url;

const SOCKS_SERVERS: &[&str] = &[
    "socks-us34.nordvpn.com",
    "socks-us35.nordvpn.com",
    "socks-us36.nordvpn.com",
    "socks-us37.nordvpn.com",
    "socks-us45.nordvpn.com",
    "socks-us46.nordvpn.com",
    "socks-us47.nordvpn.com",
    "socks-us48.nordvpn.com",
    "socks-us49.nordvpn.com",
    "socks-us50.nordvpn.com",
    "socks-us28.nordvpn.com",
    "socks-us29.nordvpn.com",
    "socks-us30.nordvpn.com",
    "socks-us31.nordvpn.com",
    "socks-us32.nordvpn.com",
    "socks-us33.nordvpn.com",
];
const MIN_LANES: usize = 8;
const MAX_LANES: usize = 10;
const DEFAULT_BASE_PORT: u16 = 18_600;
const UPSTREAM_PORT: u16 = 1080;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const EGRESS_PROBE_TIMEOUT: Duration = Duration::from_secs(6);
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(120);
const ROTATION_DRAIN_TIMEOUT: Duration = Duration::from_secs(120);
const ROTATION_CONTROL_TIMEOUT: Duration = Duration::from_secs(300);
const ROTATE_REASONS: &[&str] = &["rate-limit", "proactive", "manual"];

#[derive(Parser)]
#[command(name = "nord-socks-egress", version, about)]
struct Cli {
    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Subcommand)]
enum CommandKind {
    /// Start the private local relay daemon.
    Start,
    /// Start the relay and print safe shell exports for Headroom.
    Env,
    /// Print the newline-delimited Headroom proxy pool.
    Pool,
    /// Show the current lane-to-endpoint mapping.
    Status,
    /// Probe every lane and print egress fingerprints only.
    Test,
    /// Stop the local relay daemon.
    Stop,
    /// Rotate one opaque Headroom egress ID.
    Rotate { egress_id: String, reason: String },
    #[command(hide = true)]
    Daemon,
}

#[derive(Clone)]
struct Credentials {
    username: Vec<u8>,
    password: Vec<u8>,
}

#[derive(Clone)]
struct LaneSnapshot {
    upstream_index: usize,
    upstream_address: String,
    exit_ip: String,
    generation: u64,
}

struct LaneState {
    upstream_index: usize,
    upstream_address: String,
    exit_ip: String,
    generation: u64,
    connects: u64,
    rotating: bool,
}

struct Lane {
    slot: usize,
    upstream_port: u16,
    credentials: Credentials,
    state: Mutex<LaneState>,
    active: Mutex<HashMap<u64, Vec<Arc<TcpStream>>>>,
    next_connection: AtomicU64,
}

struct Manager {
    base_port: u16,
    lanes: Mutex<Vec<Arc<Lane>>>,
    rotation_lock: Mutex<()>,
    running: Arc<AtomicBool>,
}

struct DaemonLock {
    path: PathBuf,
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn expand_home(path: PathBuf) -> PathBuf {
    if path == Path::new("~") {
        home_dir()
    } else if let Some(rest) = path.to_str().and_then(|s| s.strip_prefix("~/")) {
        home_dir().join(rest)
    } else {
        path
    }
}

fn credentials_path() -> PathBuf {
    std::env::var_os("HEADROOM_NORD_SOCKS_CREDENTIALS_FILE")
        .map(PathBuf::from)
        .map(expand_home)
        .unwrap_or_else(|| home_dir().join(".config/headroom/nord-socks-credentials.json"))
}

fn state_dir() -> PathBuf {
    std::env::var_os("HEADROOM_NORD_SOCKS_STATE_DIR")
        .map(PathBuf::from)
        .map(expand_home)
        .unwrap_or_else(|| home_dir().join(".local/state/headroom/nord-socks-pool"))
}

fn base_port() -> Result<u16, String> {
    let port = std::env::var("HEADROOM_NORD_SOCKS_BASE_PORT")
        .ok()
        .map(|value| {
            value
                .parse::<u16>()
                .map_err(|_| "invalid HEADROOM_NORD_SOCKS_BASE_PORT".to_string())
        })
        .transpose()?
        .unwrap_or(DEFAULT_BASE_PORT);
    if port == 0 || port.checked_add(MAX_LANES as u16 - 1).is_none() {
        return Err("HEADROOM_NORD_SOCKS_BASE_PORT is outside the valid range".to_string());
    }
    Ok(port)
}

fn ensure_state_dir() -> Result<PathBuf, String> {
    let path = state_dir();
    fs::create_dir_all(&path).map_err(|_| "could not create private relay state directory")?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
        .map_err(|_| "could not secure relay state directory")?;
    let metadata = fs::metadata(&path).map_err(|_| "could not inspect relay state directory")?;
    // SAFETY: geteuid has no preconditions and does not retain pointers.
    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err("relay state directory must be owned by this user and mode 0700".to_string());
    }
    Ok(path)
}

fn control_socket_path() -> Result<PathBuf, String> {
    Ok(ensure_state_dir()?.join("control.sock"))
}

fn pool_url(base_port: u16, slot: usize) -> String {
    format!("socks5h://127.0.0.1:{}", u32::from(base_port) + slot as u32)
}

fn pool_urls(base_port: u16, count: usize) -> Vec<String> {
    (0..count).map(|slot| pool_url(base_port, slot)).collect()
}

fn egress_id(url: &str) -> String {
    let digest = Sha256::digest(url.as_bytes());
    format!("proxy-{}", hex::encode(&digest[..6]))
}

fn startup_lane_count(verified_count: usize) -> Result<usize, String> {
    if verified_count < MIN_LANES {
        return Err(format!(
            "only {verified_count} distinct Nord SOCKS exits verified; need at least {MIN_LANES}"
        ));
    }
    Ok(if verified_count >= MAX_LANES {
        MAX_LANES
    } else {
        MIN_LANES
    })
}

fn load_credentials() -> Result<Credentials, String> {
    let path = credentials_path();
    let metadata = fs::metadata(&path).map_err(|_| "could not read Nord SOCKS credentials file")?;
    // SAFETY: geteuid has no preconditions and does not retain pointers.
    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err("credential file must be owned by this user and mode 0600".to_string());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "credential file has no parent directory".to_string())?;
    let parent_metadata =
        fs::metadata(parent).map_err(|_| "could not inspect credential directory")?;
    if parent_metadata.mode() & 0o077 != 0 {
        return Err("credential directory must be private (mode 0700)".to_string());
    }
    let text = fs::read_to_string(&path).map_err(|_| "credential file must be UTF-8 text")?;
    let mut values = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| "expected exactly one USERNAME= and PASSWORD= line".to_string())?;
        let key = key.trim();
        if !matches!(key, "USERNAME" | "PASSWORD") || values.contains_key(key) {
            return Err("expected exactly one USERNAME= and PASSWORD= line".to_string());
        }
        values.insert(key.to_string(), value.trim().as_bytes().to_vec());
    }
    let username = values
        .remove("USERNAME")
        .ok_or_else(|| "credential file is missing USERNAME".to_string())?;
    let password = values
        .remove("PASSWORD")
        .ok_or_else(|| "credential file is missing PASSWORD".to_string())?;
    if username.is_empty() || password.is_empty() || username.len() > 255 || password.len() > 255 {
        return Err("credentials must be nonempty and at most 255 bytes".to_string());
    }
    Ok(Credentials { username, password })
}

fn resolve_server(host: &str) -> Result<String, String> {
    (host, UPSTREAM_PORT)
        .to_socket_addrs()
        .map_err(|_| "Nord SOCKS server DNS lookup failed".to_string())?
        .find(SocketAddr::is_ipv4)
        .map(|address| address.ip().to_string())
        .ok_or_else(|| "Nord SOCKS server has no IPv4 address".to_string())
}

fn probe_exit(
    upstream_index: usize,
    address: &str,
    credentials: &Credentials,
) -> Result<String, String> {
    probe_exit_via_local_lane(
        upstream_index,
        address,
        UPSTREAM_PORT,
        credentials,
        "https://api.ipify.org",
    )
}

fn probe_exit_via_local_lane(
    upstream_index: usize,
    address: &str,
    upstream_port: u16,
    credentials: &Credentials,
    probe_url: &str,
) -> Result<String, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|_| "could not bind local SOCKS probe listener")?;
    let local_address = listener
        .local_addr()
        .map_err(|_| "could not inspect local SOCKS probe listener")?;
    let client = socks_http_client(
        &format!("socks5h://{local_address}"),
        EGRESS_PROBE_TIMEOUT,
        EGRESS_PROBE_TIMEOUT,
    )?;
    let lane = Arc::new(Lane::new(
        0,
        upstream_port,
        upstream_index,
        address.to_string(),
        String::new(),
        credentials.clone(),
    ));
    let worker_lane = lane.clone();
    let worker = thread::Builder::new()
        .name("nord-socks-probe-lane".to_string())
        .spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                worker_lane.handle(stream);
            }
        })
        .map_err(|_| "could not start local SOCKS probe worker")?;

    let result = client
        .get(probe_url)
        .send()
        .map_err(|_| "Nord SOCKS endpoint failed the public egress probe".to_string())
        .and_then(read_ipify);
    drop(client);
    let worker_result = worker.join();
    match (result, worker_result) {
        (Ok(ip), Ok(())) => Ok(ip),
        (Err(error), _) => Err(error),
        (Ok(_), Err(_)) => Err("local SOCKS probe worker failed".to_string()),
    }
}

fn read_ipify(mut response: Response) -> Result<String, String> {
    if !response.status().is_success() {
        return Err("egress probe HTTP status was not successful".to_string());
    }
    let mut body = Vec::with_capacity(64);
    response
        .by_ref()
        .take(128)
        .read_to_end(&mut body)
        .map_err(|_| "could not read egress probe response")?;
    let body = std::str::from_utf8(&body)
        .map_err(|_| "egress probe response was not UTF-8")?
        .trim();
    body.parse::<IpAddr>()
        .map(|ip| ip.to_string())
        .map_err(|_| "egress probe response was not an IP address".to_string())
}

fn validate_headroom_proxy_url(value: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|_| "rotation requires a local Headroom proxy URL")?;
    let loopback = matches!(
        url.host_str(),
        Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
    );
    if url.scheme() != "http"
        || !loopback
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("rotation requires a local Headroom proxy URL".to_string());
    }
    Ok(url)
}

fn socks_http_client(
    proxy_url: &str,
    connect_timeout: Duration,
    request_timeout: Duration,
) -> Result<Client, String> {
    let proxy = Proxy::all(proxy_url).map_err(|_| "invalid SOCKS proxy URL")?;
    Client::builder()
        .no_proxy()
        .proxy(proxy)
        .connect_timeout(connect_timeout)
        .timeout(request_timeout)
        .build()
        .map_err(|_| "could not create SOCKS probe client".to_string())
}

fn set_proxy_egress_maintenance(proxy_url: &str, id: &str, rotating: bool) -> Result<(), String> {
    let base = validate_headroom_proxy_url(proxy_url)?;
    let endpoint = format!(
        "{}/debug/zen-egresses/{id}/maintenance",
        base.as_str().trim_end_matches('/')
    );
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|_| "could not create local Headroom control client")?;
    let response: Value = client
        .post(endpoint)
        .json(&json!({"rotating": rotating}))
        .send()
        .and_then(Response::error_for_status)
        .and_then(|response| response.json::<Value>())
        .map_err(|_| "could not update Headroom egress maintenance state")?;
    if response.get("ok").and_then(Value::as_bool) != Some(true)
        || response.get("rotating").and_then(Value::as_bool) != Some(rotating)
    {
        return Err("Headroom rejected the egress maintenance update".to_string());
    }
    Ok(())
}

fn wait_for_proxy_drain(proxy_url: &str, egress_id: &str) -> Result<(), String> {
    let base = validate_headroom_proxy_url(proxy_url)?;
    let endpoint = format!("{}/debug/inflight", base.as_str().trim_end_matches('/'));
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|_| "could not create local Headroom control client")?;
    drain_egress(
        egress_id,
        ROTATION_DRAIN_TIMEOUT,
        Duration::from_millis(100),
        || {
            client
                .get(&endpoint)
                .send()
                .and_then(Response::error_for_status)
                .and_then(|response| response.json::<Value>())
                .map_err(|_| "Headroom in-flight endpoint is unavailable".to_string())
        },
    )
}

/// Poll `/debug/inflight` until `egress_id` has nothing in flight. A failed
/// poll only means "not drained yet": one slow answer from a busy proxy must
/// not abandon a rotation that still has most of its deadline left.
fn drain_egress(
    egress_id: &str,
    timeout: Duration,
    interval: Duration,
    mut poll: impl FnMut() -> Result<Value, String>,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut last_failure = None;
    loop {
        match poll().and_then(|data| active_on_egress(&data, egress_id)) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(error) => last_failure = Some(error),
        }
        if Instant::now() >= deadline {
            let mut error = "active Headroom requests did not drain before rotation".to_string();
            if let Some(failure) = last_failure {
                error.push_str(&format!(" (last poll failed: {failure})"));
            }
            return Err(error);
        }
        thread::sleep(interval);
    }
}

/// Requests holding this egress. An older proxy has no `egress_in_flight`,
/// so fall back to its global count less the turns parked in the 429 hold.
fn active_on_egress(data: &Value, egress_id: &str) -> Result<u64, String> {
    if let Some(count) = data
        .get("egress_in_flight")
        .and_then(|counts| counts.get(egress_id))
        .and_then(Value::as_u64)
    {
        return Ok(count);
    }
    let in_flight = data
        .get("in_flight")
        .and_then(Value::as_u64)
        .ok_or_else(|| "Headroom in-flight endpoint returned invalid data".to_string())?;
    let zen_held = data.get("zen_held").and_then(Value::as_u64).unwrap_or(0);
    Ok(in_flight.saturating_sub(zen_held))
}

fn fingerprint(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    hex::encode(&digest[..6])
}

fn read_socks_address<R: Read>(reader: &mut R, address_type: u8) -> io::Result<Vec<u8>> {
    match address_type {
        1 => read_exact_vec(reader, 4 + 2),
        4 => read_exact_vec(reader, 16 + 2),
        3 => {
            let length = read_exact_vec(reader, 1)?;
            let mut address = length.clone();
            address.extend(read_exact_vec(reader, usize::from(length[0]) + 2)?);
            Ok(address)
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid SOCKS address type",
        )),
    }
}

fn read_exact_vec<R: Read>(reader: &mut R, length: usize) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn trace_upstream<T>(lane: usize, stage: &'static str, result: io::Result<T>) -> io::Result<T> {
    result.inspect_err(|error| {
        if std::env::var_os("HEADROOM_NORD_SOCKS_TRACE").is_some() {
            eprintln!(
                "upstream SOCKS I/O failed lane={lane} stage={stage} kind={:?}",
                error.kind()
            );
        }
    })
}

fn read_socks_request<R: Read>(reader: &mut R) -> io::Result<Vec<u8>> {
    let head = read_exact_vec(reader, 4)?;
    if head[0] != 5 || head[1] != 1 || head[2] != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported SOCKS request",
        ));
    }
    let mut request = head.clone();
    request.extend(read_socks_address(reader, head[3])?);
    Ok(request)
}

fn socks_destination(request: &[u8]) -> Option<(String, u16)> {
    let address_type = *request.get(3)?;
    let (host, port_offset) = match address_type {
        1 => {
            let bytes = request.get(4..8)?;
            (
                IpAddr::from(<[u8; 4]>::try_from(bytes).ok()?).to_string(),
                8,
            )
        }
        4 => {
            let bytes = request.get(4..20)?;
            (
                IpAddr::from(<[u8; 16]>::try_from(bytes).ok()?).to_string(),
                20,
            )
        }
        3 => {
            let length = usize::from(*request.get(4)?);
            let bytes = request.get(5..5 + length)?;
            (std::str::from_utf8(bytes).ok()?.to_string(), 5 + length)
        }
        _ => return None,
    };
    let port = u16::from_be_bytes(request.get(port_offset..port_offset + 2)?.try_into().ok()?);
    Some((host, port))
}

fn send_socks_failure(stream: &mut TcpStream, reply: u8) {
    let _ = stream.write_all(&[5, reply, 0, 1, 0, 0, 0, 0, 0, 0]);
}

impl Lane {
    fn new(
        slot: usize,
        upstream_port: u16,
        upstream_index: usize,
        upstream_address: String,
        exit_ip: String,
        credentials: Credentials,
    ) -> Self {
        Self {
            slot,
            upstream_port,
            credentials,
            state: Mutex::new(LaneState {
                upstream_index,
                upstream_address,
                exit_ip,
                generation: 0,
                connects: 0,
                rotating: false,
            }),
            active: Mutex::new(HashMap::new()),
            next_connection: AtomicU64::new(1),
        }
    }

    fn begin_connection(&self, client: &TcpStream) -> io::Result<Option<(u64, LaneSnapshot)>> {
        let state = lock_unpoisoned(&self.state);
        if state.rotating {
            return Ok(None);
        }
        let id = self.next_connection.fetch_add(1, Ordering::Relaxed);
        let socket = Arc::new(client.try_clone()?);
        lock_unpoisoned(&self.active).insert(id, vec![socket]);
        Ok(Some((
            id,
            LaneSnapshot {
                upstream_index: state.upstream_index,
                upstream_address: state.upstream_address.clone(),
                exit_ip: state.exit_ip.clone(),
                generation: state.generation,
            },
        )))
    }

    fn track_upstream(&self, id: u64, upstream: &TcpStream) -> io::Result<()> {
        lock_unpoisoned(&self.active)
            .get_mut(&id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Interrupted, "connection retired"))?
            .push(Arc::new(upstream.try_clone()?));
        Ok(())
    }

    fn unregister(&self, id: u64) {
        lock_unpoisoned(&self.active).remove(&id);
    }

    fn close_active(&self) {
        let sockets: Vec<_> = lock_unpoisoned(&self.active)
            .values()
            .flat_map(|sockets| sockets.iter().cloned())
            .collect();
        for socket in sockets {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }

    fn handle(self: Arc<Self>, mut client: TcpStream) {
        let started = match self.begin_connection(&client) {
            Ok(Some(started)) => started,
            Ok(None) => {
                let _ = client.write_all(&[5, 0xff]);
                return;
            }
            Err(_) => return,
        };
        let (connection_id, snapshot) = started;
        let result = self.handle_connected(connection_id, &snapshot, &mut client);
        if let Err(error) = result {
            // Never print I/O or reqwest errors: they may contain connection
            // details. Lane and error kind are enough to diagnose this relay.
            eprintln!(
                "relay lane={} host={} failed ({:?})",
                self.slot,
                SOCKS_SERVERS[snapshot.upstream_index],
                error.kind()
            );
            send_socks_failure(&mut client, 1);
        }
        self.unregister(connection_id);
    }

    fn handle_connected(
        &self,
        connection_id: u64,
        snapshot: &LaneSnapshot,
        client: &mut TcpStream,
    ) -> io::Result<()> {
        client.set_read_timeout(Some(CONNECT_TIMEOUT))?;
        client.set_write_timeout(Some(CONNECT_TIMEOUT))?;
        let mut version_and_count = [0_u8; 2];
        client.read_exact(&mut version_and_count)?;
        let methods = read_exact_vec(client, usize::from(version_and_count[1]))?;
        if version_and_count[0] != 5 || !methods.contains(&0) {
            client.write_all(&[5, 0xff])?;
            return Ok(());
        }
        client.write_all(&[5, 0])?;
        let request = read_socks_request(client)?;

        let mut upstream = trace_upstream(
            self.slot,
            "tcp-connect",
            TcpStream::connect_timeout(
                &SocketAddr::new(
                    snapshot.upstream_address.parse().map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidInput, "bad upstream IP")
                    })?,
                    self.upstream_port,
                ),
                CONNECT_TIMEOUT,
            ),
        )?;
        upstream.set_read_timeout(Some(CONNECT_TIMEOUT))?;
        upstream.set_write_timeout(Some(CONNECT_TIMEOUT))?;
        self.track_upstream(connection_id, &upstream)?;

        trace_upstream(self.slot, "method-write", upstream.write_all(&[5, 1, 2]))?;
        if trace_upstream(self.slot, "method-read", read_exact_vec(&mut upstream, 2))? != [5, 2] {
            if std::env::var_os("HEADROOM_NORD_SOCKS_TRACE").is_some() {
                eprintln!(
                    "upstream SOCKS authentication method unavailable lane={}",
                    self.slot
                );
            }
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SOCKS auth method rejected",
            ));
        }
        let mut auth = Vec::with_capacity(
            self.credentials.username.len() + self.credentials.password.len() + 3,
        );
        auth.push(1);
        auth.push(self.credentials.username.len() as u8);
        auth.extend_from_slice(&self.credentials.username);
        auth.push(self.credentials.password.len() as u8);
        auth.extend_from_slice(&self.credentials.password);
        trace_upstream(self.slot, "auth-write", upstream.write_all(&auth))?;
        auth.fill(0);
        if trace_upstream(self.slot, "auth-read", read_exact_vec(&mut upstream, 2))? != [1, 0] {
            if std::env::var_os("HEADROOM_NORD_SOCKS_TRACE").is_some() {
                eprintln!("upstream SOCKS authentication rejected lane={}", self.slot);
            }
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SOCKS auth rejected",
            ));
        }

        trace_upstream(self.slot, "connect-write", upstream.write_all(&request))?;
        let response_head = trace_upstream(
            self.slot,
            "connect-read-head",
            read_exact_vec(&mut upstream, 4),
        )?;
        if response_head[0] != 5 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid SOCKS response",
            ));
        }
        let response_tail = trace_upstream(
            self.slot,
            "connect-read-address",
            read_socks_address(&mut upstream, response_head[3]),
        )?;
        if std::env::var_os("HEADROOM_NORD_SOCKS_TRACE").is_some() {
            eprintln!(
                "upstream SOCKS CONNECT response lane={} reply={} reserved={} atyp={} address_bytes={} normalized_bound_address=true",
                self.slot,
                response_head[1],
                response_head[2],
                response_head[3],
                response_tail.len()
            );
        }
        // Keep upstream BND.ADDR details away from local clients: strict SOCKS
        // clients reject malformed bound addresses even on CONNECT success.
        // CONNECT clients do not use the bound endpoint, so preserve REP and
        // return a canonical unspecified IPv4 address instead.
        let client_response = [5, response_head[1], 0, 1, 0, 0, 0, 0, 0, 0];
        client.write_all(&client_response)?;
        if response_head[1] != 0 {
            if std::env::var_os("HEADROOM_NORD_SOCKS_TRACE").is_some() {
                let destination = socks_destination(&request)
                    .map(|(host, port)| format!("{host}:{port}"))
                    .unwrap_or_else(|| "invalid".to_string());
                eprintln!(
                    "upstream SOCKS CONNECT rejected lane={} destination={} reply={}",
                    self.slot, destination, response_head[1]
                );
            }
            return Ok(());
        }

        {
            let mut state = lock_unpoisoned(&self.state);
            if state.rotating || state.generation != snapshot.generation {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "lane rotated during setup",
                ));
            }
            state.connects = state.connects.saturating_add(1);
        }
        client.set_read_timeout(Some(IDLE_TIMEOUT))?;
        client.set_write_timeout(Some(IDLE_TIMEOUT))?;
        upstream.set_read_timeout(Some(IDLE_TIMEOUT))?;
        upstream.set_write_timeout(Some(IDLE_TIMEOUT))?;
        tunnel(client, &mut upstream)
    }
}

fn tunnel(client: &mut TcpStream, upstream: &mut TcpStream) -> io::Result<()> {
    let client_read = client.try_clone()?;
    let upstream_write = upstream.try_clone()?;
    let upstream_read = upstream.try_clone()?;
    let client_write = client.try_clone()?;
    let first = thread::Builder::new()
        .name("nord-socks-upstream-write".to_string())
        .spawn(move || copy_stream(client_read, upstream_write))?;
    let second = thread::Builder::new()
        .name("nord-socks-client-write".to_string())
        .spawn(move || copy_stream(upstream_read, client_write))?;
    let first_result = first
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("SOCKS tunnel worker panicked")));
    let _ = client.shutdown(Shutdown::Both);
    let _ = upstream.shutdown(Shutdown::Both);
    let second_result = second
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("SOCKS tunnel worker panicked")));
    first_result.and(second_result)
}

fn copy_stream(mut source: TcpStream, mut destination: TcpStream) -> io::Result<()> {
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        match source.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(count) => destination.write_all(&buffer[..count])?,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(())
            }
            Err(error) => return Err(error),
        }
    }
}

impl Manager {
    fn new(base_port: u16) -> Self {
        Self {
            base_port,
            lanes: Mutex::new(Vec::new()),
            rotation_lock: Mutex::new(()),
            running: Arc::new(AtomicBool::new(true)),
        }
    }

    fn lane_list(&self) -> Vec<Arc<Lane>> {
        lock_unpoisoned(&self.lanes).clone()
    }

    fn status(&self) -> Value {
        let lanes: Vec<_> = self
            .lane_list()
            .into_iter()
            .map(|lane| {
                let state = lock_unpoisoned(&lane.state);
                json!({
                    "slot": lane.slot,
                    "egress_id": egress_id(&pool_url(self.base_port, lane.slot)),
                    "host": SOCKS_SERVERS[state.upstream_index],
                    "exit_fingerprint": fingerprint(&state.exit_ip),
                    "socks_connects": state.connects,
                    "rotating": state.rotating,
                })
            })
            .collect();
        json!({
            "ok": true,
            "lane_count": lanes.len(),
            "max_lanes": MAX_LANES,
            "degraded": lanes.len() < MAX_LANES,
            "base_port": self.base_port,
            "lanes": lanes,
        })
    }

    fn rotate(&self, requested_id: &str, reason: &str, proxy_url: &str) -> Value {
        if !ROTATE_REASONS.contains(&reason) {
            return json!({"ok": false, "error": "invalid reason"});
        }
        if requested_id.len() != 18
            || !requested_id.starts_with("proxy-")
            || !requested_id[6..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return json!({"ok": false, "error": "unknown egress id"});
        }
        let _rotation_guard = lock_unpoisoned(&self.rotation_lock);
        let lanes = self.lane_list();
        let Some(lane) = lanes
            .iter()
            .find(|lane| egress_id(&pool_url(self.base_port, lane.slot)) == requested_id)
            .cloned()
        else {
            return json!({"ok": false, "error": "unknown egress id"});
        };
        let old = {
            let state = lock_unpoisoned(&lane.state);
            LaneSnapshot {
                upstream_index: state.upstream_index,
                upstream_address: state.upstream_address.clone(),
                exit_ip: state.exit_ip.clone(),
                generation: state.generation,
            }
        };
        let mut active_indices = HashSet::new();
        let mut active_ips = HashSet::new();
        for other in lanes.iter().filter(|other| other.slot != lane.slot) {
            let state = lock_unpoisoned(&other.state);
            active_indices.insert(state.upstream_index);
            active_ips.insert(state.exit_ip.clone());
        }

        let credentials = lane.credentials.clone();
        for offset in 1..=SOCKS_SERVERS.len() {
            let next_index = (old.upstream_index + offset) % SOCKS_SERVERS.len();
            if active_indices.contains(&next_index) {
                continue;
            }
            let host = SOCKS_SERVERS[next_index];
            let address = match resolve_server(host) {
                Ok(address) => address,
                Err(reason) => {
                    eprintln!(
                        "rotation candidate unavailable lane={} stage=dns host={host} reason={reason}",
                        lane.slot,
                    );
                    continue;
                }
            };
            let candidate_ip = match probe_exit(next_index, &address, &credentials) {
                Ok(ip) => ip,
                Err(reason) => {
                    eprintln!(
                        "rotation candidate unavailable lane={} stage=public-egress-probe host={host} reason={reason}",
                        lane.slot,
                    );
                    continue;
                }
            };
            if candidate_ip == old.exit_ip || active_ips.contains(&candidate_ip) {
                eprintln!(
                    "rotation candidate duplicate lane={} host={host}",
                    lane.slot
                );
                continue;
            }

            if let Err(error) = set_proxy_egress_maintenance(proxy_url, requested_id, true) {
                return json!({"ok": false, "error": error});
            }
            {
                let mut state = lock_unpoisoned(&lane.state);
                state.rotating = true;
            }
            if let Err(error) = wait_for_proxy_drain(proxy_url, requested_id) {
                lock_unpoisoned(&lane.state).rotating = false;
                let reset = set_proxy_egress_maintenance(proxy_url, requested_id, false);
                return match reset {
                    Ok(()) => json!({"ok": false, "error": error}),
                    Err(_) => json!({
                        "ok": false,
                        "error": "drain failed and proxy egress remains gated; restart Headroom"
                    }),
                };
            }

            {
                let mut state = lock_unpoisoned(&lane.state);
                if state.generation != old.generation || state.upstream_index != old.upstream_index
                {
                    state.rotating = false;
                    let _ = set_proxy_egress_maintenance(proxy_url, requested_id, false);
                    return json!({"ok": false, "error": "lane changed during rotation"});
                }
                state.upstream_index = next_index;
                state.upstream_address = address;
                state.exit_ip = candidate_ip.clone();
                state.generation = state.generation.wrapping_add(1);
            }
            lane.close_active();
            lock_unpoisoned(&lane.state).rotating = false;
            if set_proxy_egress_maintenance(proxy_url, requested_id, false).is_err() {
                return json!({
                    "ok": false,
                    "error": "rotation finished but proxy egress remains gated; restart Headroom"
                });
            }
            return json!({
                "ok": true,
                "slot": lane.slot,
                "old_host": SOCKS_SERVERS[old.upstream_index],
                "new_host": host,
                "exit_fingerprint": fingerprint(&candidate_ip),
                "reason": reason,
            });
        }
        json!({"ok": false, "error": "no verified distinct Nord exit available"})
    }

    fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        for lane in self.lane_list() {
            lane.close_active();
        }
    }

    fn serve(self: Arc<Self>) -> Result<(), String> {
        let _daemon_lock = DaemonLock::acquire()?;
        let credentials = load_credentials()?;
        let mut verified = Vec::new();
        let mut seen_ips = HashSet::new();
        for (index, host) in SOCKS_SERVERS.iter().enumerate() {
            if index >= MAX_LANES && verified.len() >= MIN_LANES {
                break;
            }
            let address = match resolve_server(host) {
                Ok(address) => address,
                Err(reason) => {
                    eprintln!(
                        "startup candidate unavailable stage=dns host={host} reason={reason}"
                    );
                    continue;
                }
            };
            let exit_ip = match probe_exit(index, &address, &credentials) {
                Ok(ip) => ip,
                Err(reason) => {
                    eprintln!(
                        "startup candidate unavailable stage=public-egress-probe host={host} reason={reason}"
                    );
                    continue;
                }
            };
            if !seen_ips.insert(exit_ip.clone()) {
                eprintln!("startup candidate duplicate host={host}");
                continue;
            }
            verified.push((index, address, exit_ip));
            if verified.len() >= MAX_LANES {
                break;
            }
        }
        let lane_count = startup_lane_count(verified.len())?;
        let lanes: Vec<_> = verified
            .into_iter()
            .take(lane_count)
            .enumerate()
            .map(|(slot, (upstream_index, address, exit_ip))| {
                Arc::new(Lane::new(
                    slot,
                    UPSTREAM_PORT,
                    upstream_index,
                    address,
                    exit_ip,
                    credentials.clone(),
                ))
            })
            .collect();

        let socket_path = control_socket_path()?;
        if socket_path.exists() {
            if control_request_at(
                &socket_path,
                &json!({"op": "status"}),
                Duration::from_secs(1),
            )
            .is_ok()
            {
                return Err("a Nord SOCKS relay is already running".to_string());
            }
            fs::remove_file(&socket_path).map_err(|_| "could not remove stale relay socket")?;
        }

        let mut listeners = Vec::with_capacity(lane_count);
        for lane in &lanes {
            let address = SocketAddr::from(([127, 0, 0, 1], self.base_port + lane.slot as u16));
            let listener = TcpListener::bind(address).map_err(|_| {
                format!("could not bind local SOCKS listener for lane {}", lane.slot)
            })?;
            listener
                .set_nonblocking(true)
                .map_err(|_| "could not configure local SOCKS listener")?;
            listeners.push((lane.clone(), listener));
        }
        *lock_unpoisoned(&self.lanes) = lanes;

        let control =
            UnixListener::bind(&socket_path).map_err(|_| "could not bind relay control socket")?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
            .map_err(|_| "could not secure relay control socket")?;
        control
            .set_nonblocking(true)
            .map_err(|_| "could not configure relay control socket")?;

        for (lane, listener) in listeners {
            let running = self.running.clone();
            thread::Builder::new()
                .name(format!("nord-socks-accept-{}", lane.slot))
                .spawn(move || accept_loop(lane, listener, running))
                .map_err(|_| "could not start local SOCKS listener thread")?;
        }
        install_signal_handlers(self.running.clone());
        eprintln!("verified distinct Nord SOCKS exits; active_lanes={lane_count}/{MAX_LANES}");
        if lane_count < MAX_LANES {
            eprintln!("warning: using {lane_count} distinct Nord SOCKS lanes; up to {MAX_LANES} are supported");
        }
        eprintln!("Nord SOCKS relay ready ({lane_count}/{MAX_LANES} lanes)");

        while self.running.load(Ordering::SeqCst) {
            match control.accept() {
                Ok((stream, _)) => {
                    let manager = self.clone();
                    let _ = thread::Builder::new()
                        .name("nord-socks-control".to_string())
                        .spawn(move || handle_control(manager, stream));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        self.stop();
        drop(control);
        let _ = fs::remove_file(socket_path);
        Ok(())
    }
}

fn accept_loop(lane: Arc<Lane>, listener: TcpListener, running: Arc<AtomicBool>) {
    while running.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let lane = lane.clone();
                let _ = thread::Builder::new()
                    .name(format!("nord-socks-lane-{}", lane.slot))
                    .spawn(move || lane.handle(stream));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

fn handle_control(manager: Arc<Manager>, mut stream: UnixStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
    let response = (|| -> Result<Value, String> {
        let mut request = Vec::with_capacity(256);
        let mut byte = [0_u8; 1];
        while request.len() < 4096 {
            match stream.read(&mut byte) {
                Ok(0) => break,
                Ok(_) if byte[0] == b'\n' => break,
                Ok(_) => request.push(byte[0]),
                Err(_) => return Err("invalid request".to_string()),
            }
        }
        let request: Value = serde_json::from_slice(&request).map_err(|_| "invalid request")?;
        match request.get("op").and_then(Value::as_str) {
            Some("status") => Ok(manager.status()),
            Some("rotate") => Ok(manager.rotate(
                request
                    .get("egress_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                request
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                request
                    .get("proxy_url")
                    .and_then(Value::as_str)
                    .unwrap_or("http://127.0.0.1:8787"),
            )),
            Some("stop") => {
                manager.stop();
                Ok(json!({"ok": true}))
            }
            _ => Ok(json!({"ok": false, "error": "invalid operation"})),
        }
    })()
    .unwrap_or_else(|error| json!({"ok": false, "error": error}));
    let mut encoded = serde_json::to_vec(&response).unwrap_or_else(|_| b"{}".to_vec());
    encoded.push(b'\n');
    let _ = stream.write_all(&encoded);
}

fn control_request_at(path: &Path, request: &Value, timeout: Duration) -> Result<Value, String> {
    let mut stream = UnixStream::connect(path).map_err(|_| "relay control socket unavailable")?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|_| "could not set relay control timeout")?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|_| "could not set relay control timeout")?;
    let mut encoded = serde_json::to_vec(request).map_err(|_| "could not encode relay request")?;
    encoded.push(b'\n');
    stream
        .write_all(&encoded)
        .map_err(|_| "could not write relay request")?;
    let mut response = Vec::with_capacity(512);
    let mut byte = [0_u8; 1];
    while response.len() < 4096 {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => response.push(byte[0]),
            Err(_) => return Err("relay control request timed out".to_string()),
        }
    }
    serde_json::from_slice(&response).map_err(|_| "relay returned invalid control data".to_string())
}

fn control_request(request: &Value, timeout: Duration) -> Result<Value, String> {
    control_request_at(&control_socket_path()?, request, timeout)
}

fn status_is_supported(status: &Value) -> bool {
    let Some(lanes) = status.get("lanes").and_then(Value::as_array) else {
        return false;
    };
    let slots: Vec<_> = lanes
        .iter()
        .filter_map(|lane| lane.get("slot").and_then(Value::as_u64))
        .collect();
    status.get("ok").and_then(Value::as_bool) == Some(true)
        && status.get("lane_count").and_then(Value::as_u64) == Some(lanes.len() as u64)
        && status.get("max_lanes").and_then(Value::as_u64) == Some(MAX_LANES as u64)
        && (MIN_LANES..=MAX_LANES).contains(&lanes.len())
        && slots == (0..lanes.len() as u64).collect::<Vec<_>>()
}

fn status_base_port(status: &Value, configured: u16) -> u16 {
    status
        .get("base_port")
        .and_then(Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        // The previous helper omitted base_port from status and always used
        // the compatibility port.
        .unwrap_or(if status.get("base_port").is_some() {
            configured
        } else {
            DEFAULT_BASE_PORT
        })
}

fn ensure_running() -> Result<Value, String> {
    if let Ok(status) = control_request(&json!({"op": "status"}), Duration::from_secs(3)) {
        if status_is_supported(&status) {
            return Ok(status);
        }
        let lane_count = status
            .get("lanes")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        return Err(format!(
            "an existing Nord SOCKS relay has {lane_count} lanes; this helper requires {MIN_LANES}-{MAX_LANES}. Drain its users, run `nord-socks-egress stop`, then start again"
        ));
    }

    let state = ensure_state_dir()?;
    let log_path = state.join("relay.log");
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_path)
        .map_err(|_| "could not open private relay log")?;
    fs::set_permissions(&log_path, fs::Permissions::from_mode(0o600))
        .map_err(|_| "could not secure private relay log")?;
    let executable = std::env::current_exe().map_err(|_| "could not locate relay executable")?;
    let mut command = Command::new(executable);
    command
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            log_file
                .try_clone()
                .map_err(|_| "could not clone relay log")?,
        ))
        .stderr(Stdio::from(log_file));
    // SAFETY: setsid has no Rust-side preconditions. It gives the helper the
    // same detached-session lifecycle as the old Python daemon.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|_| "could not launch Nord SOCKS relay daemon")?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(status) = control_request(&json!({"op": "status"}), Duration::from_secs(2)) {
            if status_is_supported(&status) {
                return Ok(status);
            }
            let lane_count = status
                .get("lanes")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            return Err(format!(
                "Nord SOCKS relay reported unsupported lane count {lane_count}"
            ));
        }
        if child
            .try_wait()
            .map_err(|_| "could not inspect relay daemon")?
            .is_some()
        {
            return Err(format!(
                "Nord SOCKS relay exited during startup; inspect its private log at {}",
                log_path.display()
            ));
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "Nord SOCKS relay did not start; inspect its private log at {}",
        log_path.display()
    ))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn probe_lane(base_port: u16, slot: usize) -> Result<String, String> {
    let client = socks_http_client(
        &pool_url(base_port, slot),
        Duration::from_secs(25),
        Duration::from_secs(30),
    )
    .map_err(|_| "client configuration failed".to_string())?;
    let response = client
        .get("https://api.ipify.org")
        .send()
        .map_err(|error| format!("{:?}", error.without_url()))?;
    read_ipify(response)
        .map(|ip| fingerprint(&ip))
        .map_err(|_| "egress probe returned an invalid response".to_string())
}

fn run_probe_command(base_port: u16, slots: usize) -> i32 {
    let mut fingerprints = Vec::with_capacity(slots);
    let mut successful = true;
    // Keep diagnostics serial: this command is run by operators, not on the
    // data path, and probing eight exits at once can itself create an
    // unnecessary authentication/request burst.
    for slot in 0..slots {
        match probe_lane(base_port, slot) {
            Ok(value) => {
                fingerprints.push(value.clone());
                println!("lane {slot}: {value}");
            }
            Err(reason) => {
                successful = false;
                println!("lane {slot}: FAILED ({reason})");
            }
        }
    }
    let unique: HashSet<_> = fingerprints.iter().collect();
    println!(
        "unique successful egresses: {} / {slots} active (max {MAX_LANES})",
        unique.len()
    );
    i32::from(!(successful && unique.len() == slots))
}

fn install_signal_handlers(running: Arc<AtomicBool>) {
    let _ = thread::Builder::new()
        .name("nord-socks-signals".to_string())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async move {
                let Ok(mut terminate) = tokio::signal::unix::signal(SignalKind::terminate()) else {
                    return;
                };
                let Ok(mut interrupt) = tokio::signal::unix::signal(SignalKind::interrupt()) else {
                    return;
                };
                tokio::select! {
                    _ = terminate.recv() => {},
                    _ = interrupt.recv() => {},
                }
                running.store(false, Ordering::SeqCst);
            });
        });
}

fn process_is_alive(pid: i32) -> bool {
    // SAFETY: kill(pid, 0) only tests process existence and has no pointer args.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

impl DaemonLock {
    fn acquire() -> Result<Self, String> {
        let path = ensure_state_dir()?.join("daemon.lock");
        for _ in 0..3 {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())
                        .map_err(|_| "could not initialize relay daemon lock")?;
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let owner = fs::read_to_string(&path)
                        .ok()
                        .and_then(|text| text.trim().parse::<i32>().ok());
                    if owner.is_some_and(process_is_alive) {
                        return Err(
                            "another Nord SOCKS relay startup is already in progress".to_string()
                        );
                    }
                    let _ = fs::remove_file(&path);
                }
                Err(_) => return Err("could not acquire relay daemon lock".to_string()),
            }
        }
        Err("could not acquire relay daemon lock".to_string())
    }
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn rotate_reason_is_valid(reason: &str) -> bool {
    ROTATE_REASONS.contains(&reason)
}

fn parse_cli() -> Result<Cli, clap::Error> {
    let mut args: Vec<_> = std::env::args_os().collect();
    if args.len() == 3
        && args[1]
            .to_str()
            .is_some_and(|argument| argument.starts_with("proxy-"))
    {
        args.insert(1, "rotate".into());
    }
    Cli::try_parse_from(args)
}

fn run() -> Result<i32, String> {
    let cli = parse_cli().map_err(|error| error.to_string())?;
    let port = base_port()?;
    match cli.command {
        CommandKind::Daemon => {
            Arc::new(Manager::new(port)).serve()?;
        }
        CommandKind::Start => {
            let status = ensure_running()?;
            let count = status
                .get("lane_count")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            println!(
                "Nord SOCKS relay ready: {count}/{MAX_LANES} local lanes, state under {}",
                state_dir().display()
            );
        }
        CommandKind::Env => {
            let status = ensure_running()?;
            let count = status
                .get("lane_count")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            let active_port = status_base_port(&status, port);
            println!(
                "export HEADROOM_ZEN_HTTP_PROXY_POOL={}",
                shell_quote(&pool_urls(active_port, count).join("\n"))
            );
            if count < MAX_LANES {
                eprintln!(
                    "nord-socks-egress: using {count}/{MAX_LANES} verified exits; cap concurrent Spark fan-out at {count}"
                );
            }
            let executable = std::env::current_exe()
                .map_err(|_| "could not locate relay executable")?
                .to_string_lossy()
                .into_owned();
            println!(
                "export HEADROOM_ZEN_EGRESS_ROTATE_COMMAND={}",
                shell_quote(&executable)
            );
        }
        CommandKind::Pool => {
            let status = ensure_running()?;
            let count = status
                .get("lane_count")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            println!(
                "{}",
                pool_urls(status_base_port(&status, port), count).join("\n")
            );
        }
        CommandKind::Status => {
            let status = control_request(&json!({"op": "status"}), Duration::from_secs(3))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&status).unwrap_or_default()
            );
        }
        CommandKind::Test => {
            let status = ensure_running()?;
            let count = status
                .get("lane_count")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            return Ok(run_probe_command(status_base_port(&status, port), count));
        }
        CommandKind::Stop => {
            let response = control_request(&json!({"op": "stop"}), Duration::from_secs(3))?;
            if response.get("ok").and_then(Value::as_bool) == Some(true) {
                println!("Nord SOCKS relay stopped");
                return Ok(0);
            }
            println!("stop failed");
            return Ok(1);
        }
        CommandKind::Rotate { egress_id, reason } => {
            if !rotate_reason_is_valid(&reason) {
                return Err("reason must be rate-limit, proactive, or manual".to_string());
            }
            let response = control_request(
                &json!({
                    "op": "rotate",
                    "egress_id": egress_id,
                    "reason": reason,
                    "proxy_url": std::env::var("HEADROOM_PROXY_URL")
                        .unwrap_or_else(|_| "http://127.0.0.1:8787".to_string()),
                }),
                ROTATION_CONTROL_TIMEOUT,
            )?;
            println!("{}", serde_json::to_string(&response).unwrap_or_default());
            return Ok(i32::from(
                response.get("ok").and_then(Value::as_bool) != Some(true),
            ));
        }
    }
    Ok(0)
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("nord-socks-egress: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn drain_counts_only_the_rotating_egress() {
        let data = json!({
            "in_flight": 7,
            "zen_held": 1,
            "egress_in_flight": {"proxy-a": 0, "proxy-b": 6}
        });
        assert_eq!(active_on_egress(&data, "proxy-a"), Ok(0));
        assert_eq!(active_on_egress(&data, "proxy-b"), Ok(6));
    }

    #[test]
    fn drain_falls_back_to_the_global_count_on_an_older_proxy() {
        let data = json!({"in_flight": 3, "zen_held": 1});
        assert_eq!(active_on_egress(&data, "proxy-a"), Ok(2));
        assert!(active_on_egress(&json!({}), "proxy-a").is_err());
    }

    #[test]
    fn drain_keeps_polling_after_a_failed_poll() {
        let mut polls = vec![
            Ok(json!({"egress_in_flight": {"proxy-a": 0}})),
            Err("Headroom in-flight endpoint is unavailable".to_string()),
            Ok(json!({"egress_in_flight": {"proxy-a": 1}})),
        ];
        let result = drain_egress(
            "proxy-a",
            Duration::from_secs(5),
            Duration::from_millis(1),
            || polls.pop().expect("drain stops once the egress is idle"),
        );
        assert_eq!(result, Ok(()));
        assert!(polls.is_empty());
    }

    #[test]
    fn drain_timeout_names_the_last_poll_failure() {
        let error = drain_egress("proxy-a", Duration::ZERO, Duration::from_millis(1), || {
            Err("Headroom in-flight endpoint is unavailable".to_string())
        })
        .unwrap_err();
        assert!(error.contains("did not drain"), "{error}");
        assert!(error.contains("endpoint is unavailable"), "{error}");
    }

    #[test]
    fn startup_uses_eight_unless_all_ten_preferred_exits_verify() {
        assert_eq!(startup_lane_count(10).unwrap(), 10);
        assert_eq!(startup_lane_count(9).unwrap(), 8);
        assert_eq!(startup_lane_count(8).unwrap(), 8);
        assert!(startup_lane_count(7).is_err());
    }

    #[test]
    fn legacy_status_uses_fixed_port_and_new_status_uses_reported_port() {
        assert_eq!(status_base_port(&json!({}), 20_000), DEFAULT_BASE_PORT);
        assert_eq!(
            status_base_port(&json!({"base_port": 20_000}), DEFAULT_BASE_PORT),
            20_000
        );
    }

    #[test]
    fn ids_and_pool_urls_match_the_proxy_contract() {
        let url = pool_url(DEFAULT_BASE_PORT, 0);
        assert_eq!(url, "socks5h://127.0.0.1:18600");
        assert_eq!(egress_id(&url).len(), 18);
        assert_eq!(pool_urls(DEFAULT_BASE_PORT, 2).len(), 2);
    }

    #[test]
    fn shell_quote_round_trips_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn invalid_headroom_proxy_urls_are_rejected_before_network_calls() {
        assert!(validate_headroom_proxy_url("http://127.0.0.1:8787").is_ok());
        assert!(validate_headroom_proxy_url("http://localhost:8787").is_ok());
        assert!(validate_headroom_proxy_url("https://example.com").is_err());
        assert!(validate_headroom_proxy_url("http://example.com").is_err());
        assert!(validate_headroom_proxy_url("http://user:pass@127.0.0.1:8787").is_err());
    }

    #[test]
    fn socks_request_parser_preserves_domain_address_and_port() {
        let request = [
            5, 1, 0, 3, 11, b'e', b'x', b'a', b'm', b'p', b'l', b'e', b'.', b'c', b'o', b'm', 1,
            187,
        ];
        assert_eq!(read_socks_request(&mut &request[..]).unwrap(), request);
    }

    #[test]
    fn reqwest_socks_proxy_uses_url_credentials_without_http_proxy_env() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert_eq!(read_exact_vec(&mut stream, 3).unwrap(), [5, 1, 2]);
            stream.write_all(&[5, 2]).unwrap();
            let auth_head = read_exact_vec(&mut stream, 2).unwrap();
            let username = read_exact_vec(&mut stream, usize::from(auth_head[1])).unwrap();
            let password_len = read_exact_vec(&mut stream, 1).unwrap()[0];
            let password = read_exact_vec(&mut stream, usize::from(password_len)).unwrap();
            assert_eq!(username, b"test-user");
            assert_eq!(password, b"test-password");
            stream.write_all(&[1, 0]).unwrap();

            let request = read_socks_request(&mut stream).unwrap();
            assert_eq!(request[3], 3, "socks5h must delegate DNS to the proxy");
            stream
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
                .unwrap();
            let mut http_request = Vec::new();
            let mut byte = [0_u8; 1];
            while !http_request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                http_request.push(byte[0]);
            }
            let body = b"198.51.100.8";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        });

        let mut proxy_url = Url::parse(&format!("socks5h://127.0.0.1:{port}")).unwrap();
        proxy_url.set_username("test-user").unwrap();
        proxy_url.set_password(Some("test-password")).unwrap();
        let client = socks_http_client(
            proxy_url.as_str(),
            Duration::from_secs(2),
            Duration::from_secs(3),
        )
        .unwrap();
        let response = client.get("http://api.ipify.org/").send().unwrap();
        assert_eq!(read_ipify(response).unwrap(), "198.51.100.8");
        server.join().unwrap();
    }

    #[test]
    fn maintenance_fails_fast_and_closes_new_lane_sessions() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let lane = Arc::new(Lane::new(
            0,
            1080,
            0,
            "127.0.0.1".to_string(),
            "198.51.100.1".to_string(),
            Credentials {
                username: b"user".to_vec(),
                password: b"pass".to_vec(),
            },
        ));
        lock_unpoisoned(&lane.state).rotating = true;
        let worker_lane = lane.clone();
        let worker = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            worker_lane.handle(stream);
        });
        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client.write_all(&[5, 1, 0]).unwrap();
        let mut response = [0; 2];
        client.read_exact(&mut response).unwrap();
        assert_eq!(response, [5, 0xff]);
        worker.join().unwrap();
    }

    #[test]
    fn relay_authenticates_upstream_and_tunnels_data() {
        let upstream_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let upstream_port = upstream_listener.local_addr().unwrap().port();
        let upstream = thread::spawn(move || {
            let (mut stream, _) = upstream_listener.accept().unwrap();
            assert_eq!(read_exact_vec(&mut stream, 3).unwrap(), [5, 1, 2]);
            stream.write_all(&[5, 2]).unwrap();
            let auth_head = read_exact_vec(&mut stream, 2).unwrap();
            assert_eq!(auth_head[0], 1);
            let username = read_exact_vec(&mut stream, usize::from(auth_head[1])).unwrap();
            let password_length = read_exact_vec(&mut stream, 1).unwrap()[0];
            let password = read_exact_vec(&mut stream, usize::from(password_length)).unwrap();
            assert_eq!(username, b"user");
            assert_eq!(password, b"pass");
            stream.write_all(&[1, 0]).unwrap();
            let request = read_socks_request(&mut stream).unwrap();
            assert_eq!(&request[..4], &[5, 1, 0, 3]);
            let mut response = vec![5, 0, 0, 1, 127, 0, 0, 1, 0, 80];
            stream.write_all(&response).unwrap();
            response.fill(0);
            let payload = read_exact_vec(&mut stream, 4).unwrap();
            stream.write_all(&payload).unwrap();
        });

        let lane_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let lane_address = lane_listener.local_addr().unwrap();
        let lane = Arc::new(Lane::new(
            0,
            upstream_port,
            0,
            "127.0.0.1".to_string(),
            "198.51.100.1".to_string(),
            Credentials {
                username: b"user".to_vec(),
                password: b"pass".to_vec(),
            },
        ));
        let worker_lane = lane.clone();
        let worker = thread::spawn(move || {
            let (stream, _) = lane_listener.accept().unwrap();
            worker_lane.handle(stream);
        });

        let mut client = TcpStream::connect(lane_address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        client.write_all(&[5, 1, 0]).unwrap();
        assert_eq!(read_exact_vec(&mut client, 2).unwrap(), [5, 0]);
        let mut request = vec![5, 1, 0, 3, 11];
        request.extend_from_slice(b"example.com");
        request.extend_from_slice(&[0, 80]);
        client.write_all(&request).unwrap();
        let response_head = read_exact_vec(&mut client, 4).unwrap();
        let response_tail = read_socks_address(&mut client, response_head[3]).unwrap();
        assert_eq!(response_head, [5, 0, 0, 1]);
        assert_eq!(response_tail, [0, 0, 0, 0, 0, 0]);
        client.write_all(b"ping").unwrap();
        assert_eq!(read_exact_vec(&mut client, 4).unwrap(), b"ping");
        drop(client);
        worker.join().unwrap();
        upstream.join().unwrap();
        assert_eq!(lock_unpoisoned(&lane.state).connects, 1);
    }

    #[test]
    fn relay_normalizes_malformed_upstream_bound_address_for_reqwest() {
        let upstream_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let upstream_port = upstream_listener.local_addr().unwrap().port();
        let upstream = thread::spawn(move || {
            let (mut stream, _) = upstream_listener.accept().unwrap();
            assert_eq!(read_exact_vec(&mut stream, 3).unwrap(), [5, 1, 2]);
            stream.write_all(&[5, 2]).unwrap();
            let auth_head = read_exact_vec(&mut stream, 2).unwrap();
            let username = read_exact_vec(&mut stream, usize::from(auth_head[1])).unwrap();
            let password_length = read_exact_vec(&mut stream, 1).unwrap()[0];
            let password = read_exact_vec(&mut stream, usize::from(password_length)).unwrap();
            assert_eq!(username, b"user");
            assert_eq!(password, b"pass");
            stream.write_all(&[1, 0]).unwrap();

            let request = read_socks_request(&mut stream).unwrap();
            assert_eq!(&request[..4], &[5, 1, 0, 3]);
            // This upstream accepts CONNECT but incorrectly returns an empty
            // domain for BND.ADDR. Reqwest rejects this response as malformed.
            stream.write_all(&[5, 0, 0, 3, 0, 0, 0]).unwrap();

            let mut http_request = Vec::new();
            let mut byte = [0_u8; 1];
            while !http_request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                http_request.push(byte[0]);
            }
            let body = b"198.51.100.9";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        });

        let credentials = Credentials {
            username: b"user".to_vec(),
            password: b"pass".to_vec(),
        };
        let exit_ip = probe_exit_via_local_lane(
            0,
            "127.0.0.1",
            upstream_port,
            &credentials,
            "http://api.ipify.org/",
        )
        .unwrap();
        assert_eq!(exit_ip, "198.51.100.9");
        upstream.join().unwrap();
    }
}

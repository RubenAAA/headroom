//! Bundled CLI tool registry, installer, doctor, and passthrough commands
//! (Rust port of `headroom/binaries.py` + `headroom/cli/tools.py`).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};

type Error = Box<dyn std::error::Error>;

const REGISTRY_JSON: &str = include_str!("../../../../../headroom/tools.json");
const HEADROOM_BINARIES_MIRROR: &str = "HEADROOM_BINARIES_MIRROR";
const HEADROOM_BINARIES_CACHE: &str = "HEADROOM_BINARIES_CACHE";
const HEADROOM_BINARIES_OFFLINE: &str = "HEADROOM_BINARIES_OFFLINE";

#[derive(Debug)]
pub struct BinaryError(String);

impl BinaryError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for BinaryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BinaryError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlatformKey {
    pub os: String,
    pub arch: String,
    pub libc: String,
}

impl PlatformKey {
    pub fn key(&self) -> String {
        if self.os == "linux" {
            format!("{}-{}-{}", self.os, self.arch, self.libc)
        } else {
            format!("{}-{}", self.os, self.arch)
        }
    }
}

#[derive(Debug, Deserialize)]
struct Registry {
    tools: BTreeMap<String, ToolEntry>,
}

#[derive(Clone, Debug, Deserialize)]
struct ToolEntry {
    version: String,
    binary: Option<String>,
    source: Option<String>,
    #[serde(default)]
    assets: BTreeMap<String, Asset>,
}

#[derive(Clone, Debug, Deserialize)]
struct Asset {
    url: String,
    member: Option<String>,
    sha256: Option<String>,
}

fn registry() -> Result<Registry, Error> {
    Ok(serde_json::from_str(REGISTRY_JSON)?)
}

fn env(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn home_dir() -> PathBuf {
    if let Some(home) = env("HOME") {
        return PathBuf::from(home);
    }
    #[cfg(windows)]
    if let Some(profile) = env("USERPROFILE") {
        return PathBuf::from(profile);
    }
    PathBuf::from(".")
}

fn expanduser(value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    if value == "~" {
        return home_dir();
    }
    PathBuf::from(value)
}

fn machine_to_arch(machine: &str) -> String {
    match machine.to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" => "x86_64".to_string(),
        "aarch64" | "arm64" => "aarch64".to_string(),
        other => other.to_string(),
    }
}

fn is_musl() -> bool {
    if let Ok(out) = Command::new("ldd").arg("--version").output() {
        let mut bytes = out.stdout;
        bytes.extend(out.stderr);
        if String::from_utf8_lossy(&bytes)
            .to_ascii_lowercase()
            .contains("musl")
        {
            return true;
        }
    }
    std::fs::read_dir("/lib")
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .chain(
            std::fs::read_dir("/lib64")
                .ok()
                .into_iter()
                .flatten()
                .filter_map(Result::ok),
        )
        .any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("ld-musl-") && name.ends_with(".so.1")
        })
}

pub fn detect_platform() -> PlatformKey {
    let arch = machine_to_arch(std::env::consts::ARCH);
    if cfg!(target_os = "linux") {
        PlatformKey {
            os: "linux".to_string(),
            arch,
            libc: if is_musl() { "musl" } else { "gnu" }.to_string(),
        }
    } else if cfg!(target_os = "macos") {
        PlatformKey {
            os: "darwin".to_string(),
            arch,
            libc: "n/a".to_string(),
        }
    } else if cfg!(target_os = "windows") {
        PlatformKey {
            os: "windows".to_string(),
            arch,
            libc: "n/a".to_string(),
        }
    } else {
        PlatformKey {
            os: std::env::consts::OS.to_string(),
            arch,
            libc: "n/a".to_string(),
        }
    }
}

fn cache_dir() -> PathBuf {
    if let Some(override_path) = env(HEADROOM_BINARIES_CACHE) {
        return expanduser(&override_path);
    }
    if cfg!(target_os = "windows") {
        let base = env("LOCALAPPDATA").unwrap_or_else(|| {
            home_dir()
                .join("AppData")
                .join("Local")
                .to_string_lossy()
                .into_owned()
        });
        return PathBuf::from(base).join("headroom").join("bin");
    }
    if cfg!(target_os = "macos") {
        return home_dir()
            .join("Library")
            .join("Caches")
            .join("headroom")
            .join("bin");
    }
    env("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".cache"))
        .join("headroom")
        .join("bin")
}

fn path_lookup(name: &str, entry: Option<&ToolEntry>) -> Option<PathBuf> {
    let mut candidates = vec![name.to_string()];
    if let Some(binary) = entry.and_then(|e| e.binary.as_ref()) {
        if binary != name {
            candidates.push(binary.clone());
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for candidate in &candidates {
            let exe = if cfg!(target_os = "windows") && !candidate.ends_with(".exe") {
                format!("{candidate}.exe")
            } else {
                candidate.clone()
            };
            let full = dir.join(exe);
            if full.is_file() {
                return Some(full);
            }
        }
    }
    None
}

fn is_pypi_tool(entry: &ToolEntry) -> bool {
    entry.version == "pypi" || entry.assets.is_empty()
}

fn asset_for_platform<'a>(
    tool: &str,
    entry: &'a ToolEntry,
    plat: &PlatformKey,
) -> Result<&'a Asset, BinaryError> {
    if is_pypi_tool(entry) {
        let binary = entry.binary.as_deref().unwrap_or(tool);
        return Err(BinaryError::new(format!(
            "{tool}: distributed via PyPI only; `pip install headroom-ai` should have placed `{binary}` on PATH."
        )));
    }
    entry.assets.get(&plat.key()).ok_or_else(|| {
        let supported: Vec<&str> = entry.assets.keys().map(String::as_str).collect();
        BinaryError::new(format!(
            "{tool}: no prebuilt binary for {}; supported: {:?}",
            plat.key(),
            supported
        ))
    })
}

fn binary_name(entry: &ToolEntry, fallback: &str, plat: &PlatformKey) -> String {
    let base = entry.binary.as_deref().unwrap_or(fallback);
    if plat.os == "windows" && !base.ends_with(".exe") {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

fn cached_path(tool: &str, entry: &ToolEntry, plat: &PlatformKey) -> PathBuf {
    cache_dir()
        .join(format!("{}-{}-{}", tool, entry.version, plat.key()))
        .join(binary_name(entry, tool, plat))
}

fn mirror_url(url: &str) -> String {
    let Some(mirror) = env(HEADROOM_BINARIES_MIRROR) else {
        return url.to_string();
    };
    for prefix in [
        "https://github.com",
        "https://objects.githubusercontent.com",
    ] {
        if let Some(rest) = url.strip_prefix(prefix) {
            return format!("{}{}", mirror.trim_end_matches('/'), rest);
        }
    }
    url.to_string()
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn verify_sha256(path: &Path, expected: Option<&str>) -> Result<(), Error> {
    let Some(expected) = expected.filter(|s| !s.trim().is_empty()) else {
        return Ok(());
    };
    let got = sha256_file(path)?;
    if got.to_ascii_lowercase() != expected.to_ascii_lowercase() {
        let _ = std::fs::remove_file(path);
        return Err(BinaryError::new(format!(
            "sha256 mismatch for {}: expected {expected}, got {got}",
            path.file_name().unwrap_or_default().to_string_lossy()
        ))
        .into());
    }
    Ok(())
}

fn download(url: &str, dest: &Path) -> Result<(), Error> {
    if env(HEADROOM_BINARIES_OFFLINE).is_some() {
        return Err(BinaryError::new(format!(
            "offline mode (HEADROOM_BINARIES_OFFLINE=1) but fetch required: {url}"
        ))
        .into());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let final_url = mirror_url(url);
    // Refuse plaintext. A SHA-256 pin catches a tampered payload, but only
    // where a pin exists, and it does nothing about what an eavesdropper on
    // the way learns. Checked after mirror substitution so it covers both an
    // http:// asset URL and an http:// HEADROOM_BINARIES_MIRROR.
    if !final_url.starts_with("https://") {
        return Err(BinaryError::new(format!(
            "refusing to download over a non-https URL: {final_url}\n\
             (set {HEADROOM_BINARIES_MIRROR} to an https:// origin)"
        ))
        .into());
    }
    let mut resp = headroom_proxy::ssl_context::blocking_client_builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?
        .get(&final_url)
        .header("User-Agent", "headroom-binaries/1")
        .send()
        .map_err(|e| BinaryError::new(format!("failed to download {final_url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(BinaryError::new(format!(
            "failed to download {final_url}: HTTP {}",
            resp.status()
        ))
        .into());
    }
    let mut out = File::create(dest)?;
    io::copy(&mut resp, &mut out)?;
    Ok(())
}

fn extract_member_from_tar(archive: &Path, member: &str, dest: &Path) -> Result<(), Error> {
    let file = File::open(archive)?;
    let gz = GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);
    let wanted = member.to_ascii_lowercase();
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?;
        let base = path
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if base == wanted {
            let mut out = File::create(dest)?;
            io::copy(&mut entry, &mut out)?;
            return Ok(());
        }
    }
    Err(BinaryError::new(format!(
        "archive did not contain expected member {member:?}"
    ))
    .into())
}

fn extract_member_from_zip(archive: &Path, member: &str, dest: &Path) -> Result<(), Error> {
    let file = File::open(archive)?;
    let mut archive = zip::ZipArchive::new(file)?;
    let wanted = member.to_ascii_lowercase();
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        if file.is_dir() {
            continue;
        }
        let base = Path::new(file.name())
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if base == wanted {
            let mut out = File::create(dest)?;
            io::copy(&mut file, &mut out)?;
            return Ok(());
        }
    }
    Err(BinaryError::new(format!(
        "archive did not contain expected member {member:?}"
    ))
    .into())
}

fn extract(archive: &Path, member: &str, dest: &Path) -> Result<(), Error> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = archive
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        extract_member_from_tar(archive, member, dest)
    } else if name.ends_with(".zip") {
        extract_member_from_zip(archive, member, dest)
    } else if name.ends_with(".gz") {
        let mut gz = GzDecoder::new(File::open(archive)?);
        let mut out = File::create(dest)?;
        io::copy(&mut gz, &mut out)?;
        Ok(())
    } else {
        std::fs::copy(archive, dest)?;
        Ok(())
    }
}

fn resolve(tool: &str) -> Result<PathBuf, Error> {
    let reg = registry()?;
    let entry = reg
        .tools
        .get(tool)
        .ok_or_else(|| BinaryError::new(format!("unknown tool {tool:?}")))?;
    if let Some(path) = path_lookup(tool, Some(entry)) {
        return Ok(path);
    }
    let plat = detect_platform();
    let asset = asset_for_platform(tool, entry, &plat)?;
    let binary_path = cached_path(tool, entry, &plat);
    if binary_path.exists() {
        return Ok(binary_path);
    }

    let tmp = tempfile::Builder::new()
        .prefix("headroom-fetch-")
        .tempdir()?;
    let download_name = asset
        .url
        .split('?')
        .next()
        .unwrap_or(&asset.url)
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("download");
    let download_path = tmp.path().join(download_name);
    download(&asset.url, &download_path)?;
    verify_sha256(&download_path, asset.sha256.as_deref())?;
    let staging = tmp.path().join("out");
    let member = asset
        .member
        .as_deref()
        .map(str::to_string)
        .unwrap_or_else(|| binary_name(entry, tool, &plat));
    extract(&download_path, &member, &staging)?;
    if let Some(parent) = binary_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let partial = binary_path.with_file_name(format!(
        "{}.{}.partial",
        binary_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy(),
        std::process::id()
    ));
    move_file(&staging, &partial)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o755));
    }
    std::fs::rename(&partial, &binary_path)?;
    Ok(binary_path)
}

fn move_file(from: &Path, to: &Path) -> io::Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(18) => {
            std::fs::copy(from, to)?;
            std::fs::remove_file(from)
        }
        Err(e) => Err(e),
    }
}

// Backs a `tools which` subcommand that is not registered yet.
#[allow(dead_code)]
fn which(tool: &str) -> Result<Option<PathBuf>, Error> {
    let reg = registry()?;
    let Some(entry) = reg.tools.get(tool) else {
        return Ok(None);
    };
    if let Some(path) = path_lookup(tool, Some(entry)) {
        return Ok(Some(path));
    }
    let plat = detect_platform();
    if asset_for_platform(tool, entry, &plat).is_err() {
        return Ok(None);
    }
    let path = cached_path(tool, entry, &plat);
    Ok(path.exists().then_some(path))
}

pub fn exec_tool(tool: &str, args: Vec<OsString>) -> Result<(), Error> {
    let path = match resolve(tool) {
        Ok(path) => path,
        Err(e) => {
            eprintln!("error: {e}");
            if e.to_string().contains("offline mode") {
                eprintln!(
                    "Hint: run `headroom tools install` on a networked machine, or pass --from <bundle.tar.gz>."
                );
            }
            std::process::exit(2);
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = Command::new(&path).args(args).exec();
        return Err(err.into());
    }
    #[cfg(not(unix))]
    {
        let status = Command::new(&path).args(args).status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

pub fn cmd_list() -> Result<(), Error> {
    let reg = registry()?;
    let plat = detect_platform();
    println!("platform: {}", plat.key());
    println!("cache: {}", cache_dir().display());
    println!(
        "{:<12} {:<10} {:<34} platforms",
        "tool", "version", "source"
    );
    for (name, entry) in reg.tools {
        let platforms = if entry.assets.is_empty() {
            "(pypi)".to_string()
        } else {
            entry.assets.keys().cloned().collect::<Vec<_>>().join(", ")
        };
        println!(
            "{:<12} {:<10} {:<34} {}",
            name,
            entry.version,
            entry.source.unwrap_or_default(),
            platforms
        );
    }
    Ok(())
}

#[derive(Debug)]
struct StatusRow {
    tool: String,
    version: String,
    platform: String,
    source: String,
    path: Option<PathBuf>,
    state: String,
    detail: Option<String>,
    sha_pinned: Option<bool>,
}

fn status() -> Result<Vec<StatusRow>, Error> {
    let reg = registry()?;
    let plat = detect_platform();
    let mut rows = Vec::new();
    for (name, entry) in reg.tools {
        let mut row = StatusRow {
            tool: name.clone(),
            version: entry.version.clone(),
            platform: plat.key(),
            source: entry
                .source
                .clone()
                .unwrap_or_else(|| "fetched".to_string()),
            path: None,
            state: "missing".to_string(),
            detail: None,
            sha_pinned: None,
        };
        if let Some(path) = path_lookup(&name, Some(&entry)) {
            row.path = Some(path);
            row.state = "on-path".to_string();
            rows.push(row);
            continue;
        }
        match asset_for_platform(&name, &entry, &plat) {
            Ok(asset) => {
                row.sha_pinned = Some(asset.sha256.as_deref().is_some_and(|s| !s.is_empty()));
                let cached = cached_path(&name, &entry, &plat);
                if cached.exists() {
                    row.path = Some(cached);
                    row.state = "cached".to_string();
                }
            }
            Err(e) => {
                row.state = "unsupported-platform".to_string();
                row.detail = Some(e.to_string());
            }
        }
        rows.push(row);
    }
    Ok(rows)
}

pub fn cmd_doctor(json: bool) -> Result<i32, Error> {
    let rows = status()?;
    let broken = rows
        .iter()
        .any(|r| r.state == "missing" || r.state == "unsupported-platform");
    if json {
        let payload: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "tool": r.tool,
                    "state": r.state,
                    "version": r.version,
                    "platform": r.platform,
                    "source": r.source,
                    "path": r.path.as_ref().map(|p| p.to_string_lossy().to_string()),
                    "detail": r.detail,
                    "sha_pinned": r.sha_pinned,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(if broken { 1 } else { 0 });
    }
    println!(
        "{:<12} {:<22} {:<10} {:<20} path",
        "tool", "state", "version", "platform"
    );
    for r in &rows {
        println!(
            "{:<12} {:<22} {:<10} {:<20} {}",
            r.tool,
            r.state,
            r.version,
            r.platform,
            r.path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "-".to_string())
        );
    }
    for r in &rows {
        if let Some(detail) = &r.detail {
            println!("{}: {}", r.tool, detail);
        }
    }
    Ok(if broken { 1 } else { 0 })
}

pub fn cmd_install(tools: Vec<String>, force: bool) -> Result<i32, Error> {
    let reg = registry()?;
    let selected = if tools.is_empty() {
        reg.tools.keys().cloned().collect::<Vec<_>>()
    } else {
        tools
    };
    let mut exit_code = 0;
    for name in selected {
        let Some(entry) = reg.tools.get(&name) else {
            eprintln!("unknown tool {name:?}; skipping");
            exit_code = 1;
            continue;
        };
        if is_pypi_tool(entry) {
            if let Some(path) = path_lookup(&name, Some(entry)) {
                println!("{name}: on PATH at {} (pypi wheel)", path.display());
            } else {
                println!("{name}: not on PATH - `pip install headroom-ai` should provide it");
                exit_code = 1;
            }
            continue;
        }
        if force {
            let plat = detect_platform();
            let cached = cached_path(&name, entry, &plat);
            if cached.exists() {
                if let Err(e) = std::fs::remove_file(&cached) {
                    eprintln!("{name}: failed to remove cached binary: {e}");
                    exit_code = 1;
                }
            }
        }
        match resolve(&name) {
            Ok(path) => println!("{name}: installed -> {}", path.display()),
            Err(e) => {
                eprintln!("{name}: {e}");
                exit_code = 1;
            }
        }
    }
    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_key_formats_linux_with_libc_only() {
        assert_eq!(
            PlatformKey {
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
                libc: "gnu".to_string(),
            }
            .key(),
            "linux-x86_64-gnu"
        );
        assert_eq!(
            PlatformKey {
                os: "darwin".to_string(),
                arch: "aarch64".to_string(),
                libc: "n/a".to_string(),
            }
            .key(),
            "darwin-aarch64"
        );
    }

    #[test]
    fn registry_lookup_distinguishes_pypi_and_assets() {
        let reg = registry().unwrap();
        let ast = reg.tools.get("ast-grep").unwrap();
        assert!(is_pypi_tool(ast));
        let difft = reg.tools.get("difft").unwrap();
        assert!(!is_pypi_tool(difft));
        let plat = PlatformKey {
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            libc: "gnu".to_string(),
        };
        assert!(asset_for_platform("difft", difft, &plat).is_ok());
    }

    #[test]
    fn which_unknown_tool_is_none() {
        assert!(which("__missing_headroom_tool__").unwrap().is_none());
    }
}

#[cfg(test)]
mod https_only_download_tests {
    use super::*;

    /// The guard sits before the request is built, so this needs no network.
    #[test]
    fn plaintext_urls_are_refused() {
        let dir = std::env::temp_dir().join("headroom-https-guard-test");
        let dest = dir.join("tool");
        for url in [
            "http://example.com/tool.tar.gz",
            "ftp://example.com/tool.tar.gz",
        ] {
            let err = download(url, &dest).expect_err("must refuse plaintext");
            assert!(
                err.to_string().contains("non-https"),
                "wrong error for {url}: {err}"
            );
        }
        assert!(
            !dest.exists(),
            "nothing should have been written for a refused URL"
        );
    }
}

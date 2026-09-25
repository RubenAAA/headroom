//! The redaction key on disk, and the tokens sealed under it.
//!
//! Moved out of `redact.rs` without behavior change. `use super::*`
//! keeps the parent's items and imports in reach; the parent re-exports
//! this module, so callers keep their paths.

use super::*;

/// Where the restore key lives. `HEADROOM_REDACT_KEY_FILE` overrides it, which
/// is what the tests use so a run never touches the real one.
pub(super) fn key_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("HEADROOM_REDACT_KEY_FILE") {
        return std::path::PathBuf::from(p);
    }
    let state = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| s.starts_with('/'))
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            format!("{home}/.local/state")
        });
    std::path::PathBuf::from(state)
        .join("headroom")
        .join("redact.key")
}

/// Read the key, or create one on first use. `0600`, and `create_new` so two
/// proxies racing to start cannot write over each other — the loser re-reads
/// the winner's key rather than minting tokens the winner cannot restore.
///
/// A key that cannot be read or written is not fatal: the process falls back
/// to a random in-memory key, which restores everything it minted itself and
/// nothing from before it, exactly as the old map-only design did.
pub(super) fn load_or_create_key() -> ([u8; 32], bool) {
    let path = key_path();
    let mut key = [0u8; 32];

    if let Some(result) = read_existing_key(&path, &mut key) {
        return result;
    }

    getrandom_key(&mut key);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    persist_key(&path, key)
}

/// A readable 32-byte key file wins immediately; a malformed one warns and
/// falls through to minting. `None` means no usable file — keep going.
/// Extracted from `load_or_create_key` without behavior change.
pub(super) fn read_existing_key(
    path: &std::path::Path,
    key: &mut [u8; 32],
) -> Option<([u8; 32], bool)> {
    if let Ok(bytes) = std::fs::read(path) {
        if bytes.len() == 32 {
            key.copy_from_slice(&bytes);
            return Some((*key, true));
        }
        tracing::warn!(
            event = "redact_key_malformed",
            path = %path.display(),
            len = bytes.len(),
            "restore key is not 32 bytes; placeholders minted before now will not restore"
        );
    }
    None
}

/// Persist a minted key with `0600` and `create_new`, so two proxies racing
/// to start cannot write over each other — the loser re-reads the winner's
/// key rather than minting tokens the winner cannot restore.
/// Extracted from `load_or_create_key` without behavior change.
pub(super) fn persist_key(path: &std::path::Path, key: [u8; 32]) -> ([u8; 32], bool) {
    use std::os::unix::fs::OpenOptionsExt;

    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(f) => finish_key_write(f, path, key),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => adopt_race_winner_key(path, key),
        Err(e) => report_unavailable_key(path, key, e),
    }
}

/// Write a freshly minted key to a newly created file.
/// Extracted from `persist_key` without behavior change.
pub(super) fn finish_key_write(
    mut f: std::fs::File,
    path: &std::path::Path,
    key: [u8; 32],
) -> ([u8; 32], bool) {
    use std::io::Write;

    if let Err(e) = f.write_all(&key) {
        tracing::warn!(event = "redact_key_write_failed", error = %e);
        return (key, false);
    }
    tracing::info!(
        event = "redact_key_created",
        path = %path.display(),
        "wrote a new restore key"
    );
    (key, true)
}

/// Lost the creation race: re-read the winner's key, which is the one that
/// counts. Anything else leaves this process on its in-memory key.
/// Extracted from `persist_key` without behavior change.
pub(super) fn adopt_race_winner_key(path: &std::path::Path, key: [u8; 32]) -> ([u8; 32], bool) {
    // Lost the race. The winner's key is the one that counts.
    match std::fs::read(path) {
        Ok(bytes) if bytes.len() == 32 => {
            let mut key = key;
            key.copy_from_slice(&bytes);
            (key, true)
        }
        _ => (key, false),
    }
}

/// No key file can be read or written: fall back to the in-memory key, which
/// restores everything this process mints itself and nothing from before it.
/// Extracted from `persist_key` without behavior change.
pub(super) fn report_unavailable_key(
    path: &std::path::Path,
    key: [u8; 32],
    e: std::io::Error,
) -> ([u8; 32], bool) {
    tracing::warn!(
        event = "redact_key_unavailable",
        path = %path.display(),
        error = %e,
        "no restore key on disk; placeholders will not survive this process"
    );
    (key, false)
}

/// 32 bytes from the OS. `/dev/urandom` directly rather than through an RNG
/// crate: one read, no generic plumbing, and the same source either way.
pub(super) fn getrandom_key(out: &mut [u8; 32]) {
    use std::io::Read;
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(out)) {
        Ok(()) => {}
        Err(e) => {
            // Never mint tokens from a predictable key: without randomness the
            // placeholder is guessable, which is the one thing it must not be.
            panic!("cannot read /dev/urandom for the redaction key: {e}");
        }
    }
}

/// The key the nonce MAC runs under: derived from the redaction key, so it
/// is as durable as that key, but never the cipher key itself.
pub(super) fn nonce_key(key: &[u8; 32]) -> [u8; 32] {
    use hmac::{Hmac, KeyInit, Mac};
    let mut mac =
        Hmac::<sha2::Sha256>::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(b"headroom-redact-nonce-key");
    mac.finalize().into_bytes().into()
}

/// Mint a placeholder deterministically: the value sealed under the redaction
/// key, behind a nonce that is an HMAC over the session, kind and value,
/// NUL-separated the way `conversation_discriminator` separates its fields.
pub(super) fn token_for(
    cipher: &ChaCha20Poly1305,
    nonce_key: &[u8; 32],
    session: &str,
    kind: &str,
    original: &str,
    attempt: u32,
) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    // The nonce is derived from the value, so the same value in the same
    // session always mints the same token — the property the provider's cached
    // prefix depends on. Deriving it from the plaintext also means a nonce is
    // only ever paired with the plaintext that produced it, which is the rule
    // that makes reuse safe. The session is salted in so two sessions holding
    // the same secret still send different bytes upstream.
    //
    // The nonce goes upstream in the clear, so it is keyed. A plain hash of
    // the value let anyone holding a token test a guessed password against it
    // offline, given the session key, which is built from the conversation's
    // opening message and the client's credential.
    let mut hasher =
        Hmac::<sha2::Sha256>::new_from_slice(nonce_key).expect("HMAC takes a key of any length");
    hasher.update(b"headroom-redact-nonce");
    hasher.update(session.as_bytes());
    hasher.update(&[0u8]);
    hasher.update(kind.as_bytes());
    hasher.update(&[0u8]);
    hasher.update(original.as_bytes());
    if attempt > 0 {
        hasher.update(&[0u8]);
        hasher.update(&attempt.to_le_bytes());
    }
    let digest = hasher.finalize().into_bytes();
    let nonce_bytes: [u8; NONCE_LEN] = digest[..NONCE_LEN]
        .try_into()
        .expect("digest is longer than the nonce");
    let nonce = Nonce::from(nonce_bytes);

    // The kind is authenticated but not encrypted: it is already in the token,
    // and binding it stops a token being read back as another kind.
    let sealed = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: original.as_bytes(),
                aad: kind.as_bytes(),
            },
        )
        .expect("ChaCha20-Poly1305 seals any plaintext this scanner can produce");

    let mut body = Vec::with_capacity(NONCE_LEN + sealed.len());
    body.extend_from_slice(&digest[..NONCE_LEN]);
    body.extend_from_slice(&sealed);
    format!("{PREFIX}{kind}_{}__", hex::encode(body))
}

/// Recover what a counted token carries. `None` when the token was not minted
/// by this key — an older process's, a shorter legacy token, or an invention.
pub(super) fn value_of(cipher: &ChaCha20Poly1305, token: &str) -> Option<String> {
    let rest = token.strip_prefix(PREFIX)?.strip_suffix("__")?;
    let (kind, hex_body) = rest.split_once('_')?;
    let body = hex::decode(hex_body).ok()?;
    if body.len() <= TOKEN_OVERHEAD {
        return None;
    }
    let (nonce, sealed) = body.split_at(NONCE_LEN);
    let opened = cipher
        .decrypt(
            &Nonce::from(
                <[u8; NONCE_LEN]>::try_from(nonce).expect("split_at gave NONCE_LEN bytes"),
            ),
            Payload {
                msg: sealed,
                aad: kind.as_bytes(),
            },
        )
        .ok()?;
    String::from_utf8(opened).ok()
}

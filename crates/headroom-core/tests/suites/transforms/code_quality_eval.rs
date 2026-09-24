//! Rung 4a (local, no LLM): code-skeleton quality vs truncate baseline.
//!
//! Corpus: one realistic multi-function module per language (Python, Go,
//! Rust, TypeScript) with known answer symbols buried among distractors.
//! Metrics per sample: compression ratio, `syntax_valid`, top-level symbol
//! survival (all def/class/struct/interface names), salient-identifier
//! survival (defined + called names). The truncate baseline keeps the same
//! token budget from the top of the file — the shape naive context-capping
//! produces. Skeleton must win on symbol survival; that is the whole point
//! of structure-aware compression.

use headroom_core::transforms::code_compressor::{CodeAwareCompressor, CodeCompressorConfig};
use std::collections::BTreeSet;
use std::sync::OnceLock;

fn compressor() -> &'static CodeAwareCompressor {
    static INSTANCE: OnceLock<CodeAwareCompressor> = OnceLock::new();
    INSTANCE.get_or_init(|| CodeAwareCompressor::new(CodeCompressorConfig::default()))
}

fn tokens(s: &str) -> usize {
    s.split_whitespace().count()
}

fn truncate_to(text: &str, budget: usize) -> String {
    let mut out = Vec::new();
    let mut n = 0;
    for line in text.lines() {
        let w = line.split_whitespace().count();
        if n + w > budget && !out.is_empty() {
            break;
        }
        n += w;
        out.push(line);
    }
    out.join("\n")
}

/// Naive identifier harvest: alphabetic words of length >= 3 that are not
/// language keywords. Crude but identical for both arms, so the comparison
/// is fair.
fn identifiers(text: &str) -> BTreeSet<String> {
    const KEYWORDS: &[&str] = &[
        "def",
        "class",
        "return",
        "import",
        "from",
        "for",
        "while",
        "with",
        "None",
        "True",
        "False",
        "func",
        "package",
        "var",
        "const",
        "type",
        "struct",
        "interface",
        "fn",
        "let",
        "mut",
        "pub",
        "impl",
        "use",
        "function",
        "export",
        "interface",
        "extends",
        "new",
        "this",
    ];
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| {
            w.len() >= 3
                && w.chars()
                    .next()
                    .is_some_and(|c| c.is_alphabetic() || c == '_')
                && !KEYWORDS.contains(w)
        })
        .map(str::to_string)
        .collect()
}

fn python_src() -> &'static str {
    r#"import json
import logging

logger = logging.getLogger(__name__)

def authenticate_user(username, password, attempts=3):
    """Validate credentials against the directory."""
    for attempt in range(attempts):
        record = directory.lookup(username)
        if record is None:
            logger.warning("unknown user %s", username)
            continue
        if verify_password(password, record.hash):
            session = create_session(record.id)
            audit_log("login", username, success=True)
            return session
        audit_log("login", username, success=False)
    raise AuthError("too many attempts")

def refresh_token(session_id, ttl_seconds=3600):
    """Rotate the session token."""
    session = load_session(session_id)
    if session.expired():
        raise SessionExpired(session_id)
    session.token = generate_token(32)
    session.expires_at = now() + ttl_seconds
    persist_session(session)
    metrics.incr("token.refresh")
    return session.token

def revoke_session(session_id, reason="logout"):
    """Kill a session everywhere."""
    session = load_session(session_id)
    cache.delete(session.cache_key)
    db.execute("DELETE FROM sessions WHERE id = ?", session_id)
    audit_log("revoke", session.user, reason=reason)
    notify_devices(session.user, "session revoked")
    return True

def health_check():
    return {"ok": True}
"#
}

fn go_src() -> &'static str {
    r#"package auth

import (
	"errors"
	"time"
)

func Authenticate(username, password string) (*Session, error) {
	attempts := 3
	for i := 0; i < attempts; i++ {
		rec, err := directory.Lookup(username)
		if err != nil {
			return nil, err
		}
		if rec == nil {
			continue
		}
		if Verify(rec.Hash, password) {
			s := NewSession(rec.ID)
			Audit("login", username)
			return s, nil
		}
	}
	return nil, errors.New("too many attempts")
}

func Refresh(s *Session, ttl time.Duration) (string, error) {
	if s.Expired() {
		return "", errors.New("expired")
	}
	tok := Generate(32)
	s.Token = tok
	s.Expires = time.Now().Add(ttl)
	if err := Persist(s); err != nil {
		return "", err
	}
	return tok, nil
}

func Health() string {
	return "ok"
}
"#
}

fn rust_src() -> &'static str {
    r#"use std::collections::HashMap;
use std::time::{Duration, Instant};

pub struct SessionCache {
    entries: HashMap<String, Session>,
    default_ttl: Duration,
}

impl SessionCache {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            entries: HashMap::new(),
            default_ttl: Duration::from_secs(ttl_secs),
        }
    }

    pub fn authenticate(&mut self, user: &str, pw: &str) -> Result<String, AuthError> {
        for attempt in 0..3 {
            let rec = self.lookup(user).ok_or(AuthError::UnknownUser)?;
            if verify(pw, &rec.hash) {
                let token = generate_token();
                let expiry = Instant::now() + self.default_ttl;
                self.entries.insert(token.clone(), Session::new(user, expiry));
                self.audit(user, attempt);
                return Ok(token);
            }
        }
        Err(AuthError::Locked)
    }

    pub fn revoke(&mut self, token: &str) -> bool {
        self.entries.remove(token).is_some()
    }
}
"#
}

fn ts_src() -> &'static str {
    r#"import { Directory, Session } from "./store";

export async function authenticate(username: string, password: string): Promise<Session> {
  for (let attempt = 0; attempt < 3; attempt++) {
    const record = await Directory.lookup(username);
    if (!record) {
      continue;
    }
    if (await verifyPassword(password, record.hash)) {
      const session = await createSession(record.id);
      await auditLog("login", username, true);
      return session;
    }
    await auditLog("login", username, false);
  }
  throw new AuthError("too many attempts");
}

export async function refreshToken(sessionId: string, ttl = 3600): Promise<string> {
  const session = await loadSession(sessionId);
  if (session.expired()) {
    throw new SessionExpired(sessionId);
  }
  session.token = generateToken(32);
  session.expiresAt = Date.now() + ttl * 1000;
  await persistSession(session);
  return session.token;
}

export function healthCheck(): boolean {
  return true;
}
"#
}

/// Top-level symbols an engineer would grep for: the answer set.
fn answer_symbols(lang: &str) -> Vec<&'static str> {
    match lang {
        "python" => vec![
            "authenticate_user",
            "refresh_token",
            "revoke_session",
            "health_check",
        ],
        "go" => vec!["Authenticate", "Refresh", "Health"],
        "rust" => vec!["SessionCache", "authenticate", "revoke"],
        "ts" => vec!["authenticate", "refreshToken", "healthCheck"],
        _ => vec![],
    }
}

#[test]
fn skeleton_beats_truncate_on_symbol_survival() {
    let corpus = [
        ("python", python_src()),
        ("go", go_src()),
        ("rust", rust_src()),
        ("ts", ts_src()),
    ];
    println!(
        "{:<8} {:>8} {:>8} {:>8} {:>10} {:>10}",
        "lang", "orig_tok", "skel_tok", "trunc_tok", "sym_skel", "sym_trunc"
    );
    for (lang, src) in corpus {
        let result = compressor().compress(src);
        assert!(result.syntax_valid, "{lang}: skeleton must re-parse");
        let budget = tokens(&result.compressed);
        let truncated = truncate_to(src, budget);
        assert!(tokens(&truncated) <= budget);

        let answers = answer_symbols(lang);
        let sym_skel = answers
            .iter()
            .filter(|s| result.compressed.contains(**s))
            .count();
        let sym_trunc = answers.iter().filter(|s| truncated.contains(**s)).count();

        let orig_ids = identifiers(src);
        let skel_ids = identifiers(&result.compressed);
        let trunc_ids = identifiers(&truncated);
        let id_survival =
            skel_ids.intersection(&orig_ids).count() as f64 / orig_ids.len().max(1) as f64;
        let trunc_id_survival =
            trunc_ids.intersection(&orig_ids).count() as f64 / orig_ids.len().max(1) as f64;

        println!(
            "{:<8} {:>8} {:>8} {:>8} {:>4}/{:<5} {:>4}/{:<5} id_skel={:.2} id_trunc={:.2}",
            lang,
            tokens(src),
            budget,
            tokens(&truncated),
            sym_skel,
            answers.len(),
            sym_trunc,
            answers.len(),
            id_survival,
            trunc_id_survival
        );

        // The decision rule: at equal token budget the skeleton keeps every
        // answer symbol. Truncate is allowed to lose them (that is the
        // baseline being beaten), but the skeleton must not. Identifier
        // counts are recorded, not gated: truncate wins raw identifier
        // breadth by keeping full prefix bodies (python 0.82 vs 0.63) while
        // dropping whole tail functions (3/4 symbols). That is the intended
        // trade — dropped body detail is recoverable via retrieve/Read;
        // undiscovered symbols are not.
        assert_eq!(
            sym_skel,
            answers.len(),
            "{lang}: skeleton dropped an answer symbol"
        );
        assert!(
            sym_skel >= sym_trunc,
            "{lang}: skeleton must never show fewer symbols than truncation"
        );
    }
}

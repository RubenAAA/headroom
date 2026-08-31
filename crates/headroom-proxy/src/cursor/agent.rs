//! Spawning `cursor-agent` and turning its stdout into Anthropic SSE.
//!
//! The prompt goes in on stdin rather than as an argument. A turn carries a
//! whole transcript plus a tool policy, which runs to tens of kilobytes, and
//! passing that as a trailing argv entry is an `E2BIG` waiting to happen —
//! `ARG_MAX` is 2 MiB here, but the limit covers the whole block, environment
//! included.
//!
//! Output is read a line at a time and translated as it arrives, so the caller
//! sees the first token when the agent produces it. Buffering to process exit
//! would be fine for a model that answers in a second, and is not fine for an
//! agent that may think for half a minute.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use super::translate::Translator;

/// A scratch directory that is the agent's whole world for one conversation.
///
/// Two jobs, and the second is the one that matters.
///
/// It carries the `.cursor/mcp.json` that points the agent back at the proxy.
/// That file has to sit in the workspace — the CLI takes no flag for it — so
/// writing it into whatever directory the proxy happens to be running in would
/// clobber what was already there.
///
/// And it is somewhere with nothing in it. The built-in file tools are capped
/// read-only and told not to fire, but "told" is not "cannot", and a workspace
/// holding the user's repo is a workspace the agent can read without anyone
/// approving it. Pointed at an empty directory the built-ins have nothing to
/// find, and the MCP tools are the only route to anything real.
///
/// Dropped when the conversation closes, taking the directory with it.
#[derive(Debug)]
pub(crate) struct Workspace {
    dir: tempfile::TempDir,
}

impl Workspace {
    /// Create the scratch directory and register `url` as its only MCP server.
    pub(crate) fn create(url: Option<&str>) -> Result<Self, std::io::Error> {
        let dir = tempfile::Builder::new()
            .prefix("headroom-cursor-")
            .tempdir()?;
        if let Some(url) = url {
            let cursor = dir.path().join(".cursor");
            std::fs::create_dir_all(&cursor)?;
            std::fs::write(
                cursor.join("mcp.json"),
                serde_json::json!({"mcpServers": {"headroom": {"url": url}}}).to_string(),
            )?;
        }
        Ok(Self { dir })
    }

    pub(crate) fn path(&self) -> &std::path::Path {
        self.dir.path()
    }
}

/// Everything needed to start one turn.
#[derive(Debug, Clone)]
pub(crate) struct AgentTurn {
    /// Cursor model id, e.g. `cursor-grok-4.6-high`.
    pub model: String,
    /// What the agent sees as its workspace.
    pub workspace: std::path::PathBuf,
    /// Cursor's chat id, when continuing an existing conversation.
    pub resume: Option<String>,
    /// The prompt, already assembled.
    pub prompt: String,
    /// Register this MCP endpoint for the turn. `None` leaves the workspace's
    /// own `.cursor/mcp.json` alone.
    pub mcp_url: Option<String>,
    /// Cap on a single turn. `None` waits forever, which is only right when
    /// the caller has its own deadline.
    pub timeout: Option<Duration>,
    /// Confine the agent's own shell to Cursor's sandbox.
    ///
    /// This used to be `read_only`, which spent `--mode ask` on the job. Ask
    /// mode is not a tool restriction — the CLI calls it "Q&A style for
    /// explanations and questions", and it takes the agent out of its own
    /// loop. The agent then answered every turn with a plan instead of the
    /// work, said so in its reasoning, and the conversation went in circles.
    /// The workspace is an empty scratch directory, so the built-in file tools
    /// have nothing to reach anyway; what needed containing was the shell, and
    /// `--sandbox` is the flag for that.
    pub sandbox: bool,
}

impl AgentTurn {
    fn args(&self) -> Vec<String> {
        let mut args = vec![
            "-p".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--model".into(),
            self.model.clone(),
            // Without this the agent stops to ask about the workspace and the
            // turn deadlocks: there is no terminal to answer on.
            "--trust".into(),
        ];
        if self.mcp_url.is_some() {
            args.push("--approve-mcps".into());
        }
        if self.sandbox {
            args.push("--sandbox".into());
            args.push("enabled".into());
        }
        if let Some(chat) = &self.resume {
            args.push("--resume".into());
            args.push(chat.clone());
        }
        args
    }
}

/// A running turn: the child, and the translated frames it has produced.
pub(crate) struct RunningTurn {
    child: Child,
    reader: BufReader<tokio::process::ChildStdout>,
    pub(crate) translator: Translator,
    done: bool,
    /// When [`AgentTurn::timeout`] runs out, as an absolute instant.
    ///
    /// Absolute rather than a per-read duration: the cap is on the turn, and a
    /// per-read timer would restart on every line an agent emits, so a talkative
    /// agent could run forever without ever tripping it.
    deadline: Option<tokio::time::Instant>,
}

/// Start the agent and write the prompt to its stdin.
///
/// `binary` is taken rather than hardcoded so tests can point at a stub that
/// replays a recorded transcript, and so an operator can pin a path when the
/// CLI is not on the proxy's `PATH` — it runs as a service and will not have
/// the login shell's.
pub(crate) async fn spawn(
    binary: &str,
    turn: &AgentTurn,
) -> Result<RunningTurn, std::io::Error> {
    let mut cmd = Command::new(binary);
    cmd.args(turn.args())
        .current_dir(&turn.workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Inherited, so the agent's own diagnostics land in the proxy's log.
        //
        // This is where a failure that never reaches `stream-json` shows up —
        // an expired login, a model id the account cannot use, a usage error.
        // Discarding it leaves those cases indistinguishable from a clean exit
        // with no output, and the caller sees only "ended without a result".
        //
        // Inheriting rather than piping because a pipe nobody drains fills at
        // 64 KiB and blocks the child, and draining it means a second reader
        // task for output that belongs in the log anyway.
        .stderr(Stdio::inherit())
        .kill_on_drop(true);

    let mut child = cmd.spawn()?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("cursor-agent stdin was not piped"))?;
    let prompt = turn.prompt.clone();
    // Written from a task, not inline: a prompt larger than the pipe buffer
    // (64 KiB here) blocks until the child reads, and the child does not
    // finish reading until we stop writing.
    tokio::spawn(async move {
        let _ = stdin.write_all(prompt.as_bytes()).await;
        let _ = stdin.shutdown().await;
    });

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("cursor-agent stdout was not piped"))?;

    Ok(RunningTurn {
        child,
        reader: BufReader::new(stdout),
        translator: Translator::new(turn.model.clone()),
        done: false,
        deadline: turn.timeout.map(|d| tokio::time::Instant::now() + d),
    })
}

impl RunningTurn {
    /// The next batch of SSE frames, or `None` once the turn is over.
    ///
    /// Returns a batch rather than a frame because one source event routinely
    /// produces several — an `assistant` event alone yields four.
    pub(crate) async fn next_frames(&mut self) -> Option<Vec<String>> {
        if self.done {
            return None;
        }
        loop {
            let mut line = String::new();
            let read = match self.deadline {
                Some(at) => match tokio::time::timeout_at(at, self.reader.read_line(&mut line))
                    .await
                {
                    Ok(read) => read,
                    Err(_) => {
                        // The cap the caller asked for. Kill the child rather
                        // than leaving it to `kill_on_drop`, so the process is
                        // gone before the salvaged frames reach the client and
                        // the turn cannot keep writing after it has ended.
                        tracing::warn!(
                            event = "cursor_agent_turn_timeout",
                            "cursor-agent exceeded its turn cap; killing the child"
                        );
                        let _ = self.child.start_kill();
                        self.done = true;
                        let frames = self.translator.finish_unterminated();
                        return if frames.is_empty() { None } else { Some(frames) };
                    }
                },
                None => self.reader.read_line(&mut line).await,
            };
            match read {
                Ok(0) => {
                    // EOF. Salvage a stream that stopped mid-block so the
                    // caller is not left waiting for `message_stop`.
                    self.done = true;
                    let frames = self.translator.finish_unterminated();
                    return if frames.is_empty() { None } else { Some(frames) };
                }
                Ok(_) => {
                    let frames = self.translator.push_line(&line);
                    if !frames.is_empty() {
                        return Some(frames);
                    }
                    // A line we had no mapping for. Keep reading rather than
                    // handing the caller an empty batch it would have to
                    // distinguish from the end.
                }
                Err(e) => {
                    tracing::warn!(
                        event = "cursor_agent_read_error",
                        cause = ?e,
                        "reading cursor-agent stdout"
                    );
                    self.done = true;
                    let frames = self.translator.finish_unterminated();
                    return if frames.is_empty() { None } else { Some(frames) };
                }
            }
        }
    }

    /// Reap the child. Safe to call more than once.
    pub(crate) async fn finish(&mut self) -> Option<std::process::ExitStatus> {
        let _ = self.child.start_kill();
        self.child.wait().await.ok()
    }
}

/// [`spawn`], retrying the one failure a test stub provokes that says nothing
/// about the code under test.
///
/// Writing an executable opens a window in which a `fork` on another test
/// thread inherits the descriptor it was written through. The kernel then
/// refuses to `exec` that inode with `ETXTBSY` until the forked child reaches
/// its own `exec` and closes the inherited copy. Nothing on this side can
/// prevent the inheritance, so wait it out.
#[cfg(test)]
pub(crate) async fn spawn_stub(
    binary: &str,
    turn: &AgentTurn,
) -> Result<RunningTurn, std::io::Error> {
    for _ in 0..100 {
        match spawn(binary, turn).await {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            other => return other,
        }
    }
    spawn(binary, turn).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(prompt: &str) -> AgentTurn {
        AgentTurn {
            model: "cursor-grok-4.6-high".into(),
            workspace: std::env::temp_dir(),
            resume: None,
            prompt: prompt.into(),
            mcp_url: None,
            timeout: None,
            sandbox: false,
        }
    }

    #[test]
    fn the_command_line_carries_what_a_headless_turn_needs() {
        let args = turn("hi").args();
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"stream-json".to_string()));
        // Without --trust the agent prompts about the workspace and a headless
        // turn deadlocks with no terminal to answer on.
        assert!(args.contains(&"--trust".to_string()));
        // Nothing was asked for, so nothing extra is passed.
        assert!(!args.contains(&"--approve-mcps".to_string()));
        assert!(!args.contains(&"--resume".to_string()));
        assert!(!args.contains(&"--sandbox".to_string()));
        assert!(
            !args.contains(&"--mode".to_string()),
            "ask and plan both take the agent out of its own loop"
        );
    }

    #[test]
    fn asking_for_mcp_or_resume_or_sandbox_adds_exactly_those_flags() {
        let mut t = turn("hi");
        t.mcp_url = Some("http://127.0.0.1:1/mcp".into());
        t.resume = Some("chat-7".into());
        t.sandbox = true;
        let args = t.args();
        assert!(args.contains(&"--approve-mcps".to_string()));
        assert!(args.contains(&"--sandbox".to_string()));
        assert!(args.contains(&"enabled".to_string()));
        assert!(
            !args.contains(&"--mode".to_string()),
            "containment must not cost the agent loop"
        );
        let at = args.iter().position(|a| a == "--resume").expect("--resume");
        assert_eq!(args[at + 1], "chat-7", "the chat id follows its flag");
    }

    /// End to end over a real pipe, with a stub standing in for the CLI so the
    /// test neither needs a subscription nor costs a token. What is being
    /// checked is the plumbing: prompt in on stdin, frames out in order,
    /// `None` at the end, child reaped.
    /// The cap is on the turn, and an agent that never speaks must still hit it.
    #[tokio::test]
    async fn a_silent_agent_is_cut_off_at_its_turn_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = dir.path().join("stub-agent");
        // Reads the prompt, says nothing, and would outlive the test.
        std::fs::write(&stub, "#!/bin/sh\ncat > /dev/null\nsleep 60\n").expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
                .expect("chmod stub");
        }

        let mut turn = turn("hi");
        turn.timeout = Some(Duration::from_millis(150));

        let started = std::time::Instant::now();
        let mut running = spawn(stub.to_str().unwrap(), &turn).await.expect("spawn stub");

        // The cap salvages rather than truncating: a client that has been
        // handed a `message_start` is owed a `message_stop`, and hanging up
        // silently leaves it waiting on a turn that is already dead.
        let salvaged: String = running
            .next_frames()
            .await
            .expect("the cap should close the stream, not drop it")
            .concat();
        assert!(
            salvaged.contains("message_stop"),
            "the salvaged tail was: {salvaged}"
        );
        // And the turn is over, not merely paused.
        assert!(running.next_frames().await.is_none());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the cap did not fire; the read waited on the child instead"
        );
        running.finish().await;
    }

    #[tokio::test]
    async fn a_replayed_transcript_streams_through_and_terminates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = dir.path().join("stub-agent");
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/cursor/mcp-tool-turn.jsonl"
        );
        std::fs::write(
            &stub,
            format!("#!/bin/sh\ncat > /dev/null\nexec cat {fixture}\n"),
        )
        .expect("write stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
                .expect("chmod stub");
        }

        let mut running = spawn(stub.to_str().unwrap(), &turn("what is the marker?"))
            .await
            .expect("spawn stub");

        let mut all = String::new();
        while let Some(batch) = running.next_frames().await {
            for frame in batch {
                all.push_str(&frame);
            }
        }
        running.finish().await;

        assert!(all.starts_with("event: message_start\n"));
        assert!(
            all.trim_end().ends_with(r#""type":"message_stop"}"#),
            "tail was: {}",
            &all[all.len().saturating_sub(120)..]
        );
        assert!(all.contains("CRIMSON-42"), "the answer came through");
        assert_eq!(
            running.translator.session_id.as_deref(),
            Some("cf8812c0-d9cc-4a5c-90ab-fdb08209f2b0")
        );
    }

    /// A CLI that is not installed must surface as an error, not a hang.
    #[tokio::test]
    async fn a_missing_binary_fails_immediately() {
        let err = match spawn("cursor-agent-that-is-not-installed", &turn("hi")).await {
            Err(e) => e,
            Ok(_) => panic!("a binary that does not exist must not spawn"),
        };
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    /// The real CLI, against the real service, on the user's subscription.
    ///
    /// Ignored by default: it costs tokens, needs `agent login`, and takes tens
    /// of seconds. Run it when the stream-json vocabulary might have moved —
    /// every other test in this file replays a recording and would keep passing
    /// against a format that no longer exists.
    ///
    ///   cargo test -p headroom-proxy --lib cursor::agent -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "spawns the real cursor-agent and spends tokens"]
    async fn live_cursor_agent_answers_through_the_translator() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut t = turn(
            "Reply with exactly the word ORANGE-13 and nothing else. \
             Do not use any tools.",
        );
        t.workspace = dir.path().to_path_buf();
        t.sandbox = true;

        let mut running = spawn("agent", &t).await.expect("agent must be on PATH");
        let mut all = String::new();
        while let Some(batch) = running.next_frames().await {
            for frame in batch {
                all.push_str(&frame);
            }
        }
        running.finish().await;

        eprintln!("--- {} bytes of SSE ---", all.len());
        eprintln!("{all}");
        assert!(all.starts_with("event: message_start\n"));
        assert!(all.contains("ORANGE-13"), "the model did not answer");
        assert!(all.trim_end().ends_with(r#""type":"message_stop"}"#));
        assert_eq!(running.translator.outcome, Some(super::super::translate::Outcome::EndTurn));
        assert!(
            running.translator.usage.input_tokens > 0,
            "usage did not survive: {:?}",
            running.translator.usage
        );
        assert!(running.translator.session_id.is_some(), "no session id to resume from");
    }
}

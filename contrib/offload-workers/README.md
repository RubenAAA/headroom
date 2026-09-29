# Offload workers

Provider-neutral workers for GitLab review and YouTrack filing. The detection
and handoff hooks live in `contrib/claude/hooks/`.

## Chain

```
review command runs (/gitlab-review or /fix-mr-comments → MR pinned from args)
  → user asks for replies to be drafted
  → hook spawns the worker: review_draft.py <MR> <session> <mode>
  → short handoff window for verified facts in <session>.notes.md
  → evidence is pinned to the MR head (resolve_head fetches it; refusal
    beats a confident wrong draft)
  → draft written to ~/.local/state/offload-workers/<session>.draft.json
    (flushed per thread-group; partial drafts carry head_sha + unverdicts)
  → operator reviews/edits the draft
  → explicit `review-post.sh --approve <draft>` posts it (idempotent; refuses
    if the MR head moved since the draft)
  → proof written; operator learns about it on the next prompt
```

The worker never posts automatically. The approval command is the explicit
write action; generated replies always leave threads open. A separate
`--resolve IID DISCUSSION_ID` command closes a thread only when requested.
For a one-line addendum, use `--note IID DISCUSSION_ID TEXT`.

The gate runs on prompt submission, before the model can make tool calls. If a
prompt asks for a commit or push and then a review draft, it defers the draft;
commit and push first, then request the draft in a later prompt.

## Model data handling

The default Headroom model alias currently routes to Muse Spark 1.3 Contributor
Free through OpenCode Zen.
The contributor tier permits prompts and completions to be used to train future
Meta models. A review dossier includes private thread text and relevant source
diffs, so the worker refuses to send anything unless
`OFFLOAD_ALLOW_EXTERNAL_DATA=1` is set in
`~/.config/offload-workers/env`. Set it only if that provider's data terms
are acceptable for the repository. The same gate applies to the GitLab and
ticket workers when they use the Headroom transport. Earlier versions sent to that route with no consent step, so an existing
setup drafts nothing until you set the variable once. It is deliberately
conservative: it requires consent before the worker can inspect which upstream
route Headroom will use.

Before diverting a Bash ticket write, the hook runs this same routing check. If
the selected route is not approved, it lets the original command proceed.

`OFFLOAD_MODEL` selects the model id or alias; it is independent of transport.
With `OFFLOAD_MODEL_TRANSPORT=headroom` (the default), the worker runs Claude
Code through Headroom. You can select a Claude model such as
`claude-haiku-4-5-20251001`, or a custom alias such as
`claude-union-alpha`, provided the alias is supported by Claude Code and
configured in Headroom. For OpenCode Zen or another routed provider, add the
alias to Headroom's model routes and configure that provider's credentials.
Changing `OFFLOAD_MODEL` alone does not create a missing route or credential.

For local inference, there are two paths. `OFFLOAD_MODEL_TRANSPORT=local-direct`
uses `OFFLOAD_MODEL_LOCAL_BASE_URL` and `OFFLOAD_MODEL` to call a loopback
model server directly; this bypasses Headroom and does not require
`OFFLOAD_ALLOW_EXTERNAL_DATA=1`. The endpoint must be `localhost` or a loopback
IP; port 8787 is rejected because it is Headroom's default. Set
`OFFLOAD_MODEL_LOCAL_API=anthropic` for an Anthropic Messages API (the default),
or `openai` for an OpenAI Chat Completions API such as llama.cpp's
`/v1/chat/completions` endpoint.

For a local Qwen setup matching this checkout's optional launcher path,
configure the worker with the llama.cpp server's model alias and loopback API
URL:

```sh
export OFFLOAD_MODEL_TRANSPORT=local-direct
export OFFLOAD_MODEL_LOCAL_API=openai
export OFFLOAD_MODEL_LOCAL_BASE_URL=http://127.0.0.1:8080/v1
export OFFLOAD_MODEL=qwen36-uncensored
```

Start that server separately and confirm `http://127.0.0.1:8080/v1/models`
lists the configured alias. The local launcher uses `qwen36-uncensored` as its
llama.cpp alias. The [llama.cpp server API documentation](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md)
covers its compatible endpoints. The weights and server binary are not
installed by Headroom.

Ollama v0.14.0 and later also supports the Anthropic Messages API. For a local
Ollama model, set `OFFLOAD_MODEL_LOCAL_API=anthropic`, use
`http://127.0.0.1:11434` as the base URL, and set `OFFLOAD_MODEL` to the exact
locally installed model name. Do not choose an Ollama `:cloud` model: the
loopback endpoint can still forward those requests to a cloud provider. See
[Ollama's Anthropic API guide](https://ollama.com/blog/claude). In every case,
confirm that the selected server and model run inference locally and do not
forward requests upstream. The ticket hook also checks that the loopback
listener is reachable before it blocks a manual filing command; start the
server first.

To keep a local model behind Headroom instead, configure Headroom's
`--local-model` and `--local-upstream` with a Claude Code alias and an
OpenAI-compatible local endpoint, then select that alias with the `headroom`
transport. The worker's existing consent guard still requires the opt-in on
this path because it cannot verify how Headroom routes the alias. The ticket
worker runs the YouTrack CLI itself after the model returns validated ticket
fields; the model does not need shell tools.

Muse Spark Standard is available from Meta's Model API under the model id
`muse-spark-1.3`; Meta says Standard prompts and completions are not used for
training. The current Headroom route uses OpenCode Zen's Contributor Free
variant instead. Switching to Standard would require a different API route and
Meta Model API credentials/billing; it cannot work with the current no-key
route alone. OpenCode Zen itself also requires sign-in, billing details, and
an API key for its paid routes. Either route still sends review content to an
external provider. See [OpenCode Zen setup](https://opencode.ai/docs/zen),
[Meta's model tiers](https://dev.meta.ai/docs/models), and [Meta Model API
authentication](https://dev.meta.ai/docs/quickstart).

Two survival properties, both learned from MR !597 (14 threads, ~65s each,
supervisor timeout fired one thread short):

- The drafter flushes the draft after every thread. A kill mid-loop leaves
  a partial draft (`"partial": true` + `unverdict_threads`) instead of
  nothing. The partial draft stays unposted until the operator reviews it.
- The supervisor budget is 45 minutes; each worker call keeps its own
  shorter timeout. A worker call that dies empty-handed logs its rc and
  stderr tail instead of a bare NO VERDICT.
- The ticket supervisor budget is 10 minutes. Each ticket model or CLI call
  is capped at 9 minutes, leaving time for the worker to save a failure; a
  timed-out Claude child call also records its stderr tail.

## Modes

The scope depends on which command armed the session (recorded in
`<session>.armed`):

| mode | drafts |
|---|---|
| `gitlab-review` | follow-ups on threads I opened; when I opened none yet (first-round review), new threads from the review findings instead |
| `fix-mr-comments` | reviewers' still-open threads on my own MR |

New threads (`threads` in the draft) open inline when the entry carries
file+line, plain MR-level otherwise; anchors come from the live MR
`diff_refs` at post time. Replies and new threads share the marker,
verification and proof machinery — a retry after a partial failure finishes
both without double-posting either.

## State files (`~/.local/state/offload-workers/`)

| file | means |
|---|---|
| `<session>.armed` | a review command really ran; content is the mode |
| `<session>.armed-mr` | MR iid parsed from that command's args; never guessed from transcript mentions |
| `<session>.armed-key` / `<session>.disarmed-key` | one-time command arming; a fresh slash-command invocation starts a fresh run |
| `<session>.diverted` | a divert fired, worker started |
| `<session>.drafting` | worker is drafting |
| `<session>.started` | supervisor pid; alive check before a respawn |
| `<session>.drafted` | draft is ready for review; posting has not started |
| `<session>.posting` | an explicitly approved draft is being posted |
| `<session>.posted` | poster verified the writes |
| `<session>.failed` | drafting or posting failed; reason inside |
| `<session>.worker.log` | draft run transcript and live poster progress; output also stays visible in the invoking tool call |
| `<session>.draft.json` | replies, plus `head_sha`, `partial`, and `unverdict_threads` |
| `<session>.notes.md` | hand-off facts and exact verification commands/results for the drafter |
| `<session>.proof.json` | verified note ids and markers (`ok: false` + `refused` when the MR moved); explicit resolves have a separate proof |
| `<session>.reported` | the proof already pinged; the ping fires once |

## Pieces

| file | does |
|---|---|
| `gitlab_api.py` | transport. `rtk proxy curl`, JSON bodies via `--data-binary @file` |
| `review_post.py` | poster: explicit approval, one-line note and separate resolve modes; idempotent, verified, provable |
| `review-post.sh` | runs the poster; `gitlab_api.py` reads only the non-secret settings it needs |
| `ticket_file.py` | the ticket worker: last-10-turns → model-written fields → CLI filing → proof |
| `model_call.py` | shared proxy or local model routing for both workers |
| `install-credential.sh` | moves `GITLAB_TOKEN` out of the ambient environment |
| `read_threads.py`, `triage.py`, `since_review.py`, `thread_dossier.py` | read-only analysis helpers |

## Setup

One machine-side file holds the worker configuration and YouTrack credential;
the hook sources it into workers, so interactive sessions need no extra setup:

```sh
mkdir -p ~/.config/offload-workers
cat > ~/.config/offload-workers/env <<'EOF'
export OFFLOAD_REPO=/path/to/repo-under-review
export OFFLOAD_GITLAB_BASE_URL=https://gitlab.example.com/api/v4
export OFFLOAD_GITLAB_PROJECT=group/project
export YOUTRACK_TOKEN=...
export YOUTRACK_URL=https://youtrack.example.com
export YOUTRACK_PROJECT_ID=0-00
# Multi-project alternative: leave YOUTRACK_PROJECT_ID unset and map session
# cwd prefixes to ids instead -- longest prefix wins, global still wins when
# set. Example (operator-local paths, not repo content):
# export YOUTRACK_PROJECT_MAP="$HOME/work/alpha=0-21,$HOME/work/beta=0-22"
# AI_YOUTRACK_BIN defaults to the ai-first-workspace checkout; set only to override.
# For a local model called directly (bypassing Headroom), choose:
# export OFFLOAD_MODEL_TRANSPORT=local-direct
# For llama.cpp's OpenAI-compatible API, also set:
# export OFFLOAD_MODEL_LOCAL_API=openai
# export OFFLOAD_MODEL_LOCAL_BASE_URL=http://127.0.0.1:8080/v1
# For an Anthropic Messages API such as local Ollama, use:
# export OFFLOAD_MODEL_LOCAL_API=anthropic
# export OFFLOAD_MODEL_LOCAL_BASE_URL=http://127.0.0.1:11434
# export OFFLOAD_MODEL=<model-id-served-locally>
# For a Headroom route, set OFFLOAD_MODEL to its alias instead, such as
# claude-haiku-4-5-20251001 or a configured OpenCode alias.
EOF
chmod 600 ~/.config/offload-workers/env
```

Existing `~/.config/spark-poster/env` settings remain readable. New generic
settings in `~/.config/offload-workers/env` take precedence when both files
exist. Existing state under `~/.local/state/spark-review` is accessed through
a compatibility symlink when the new state directory is first used.

The credential lives next to it: run `./install-credential.sh` to move
`GITLAB_TOKEN` out of the ambient environment into
`~/.config/offload-workers/token` (mode 600). The poster still reads an
existing token from the legacy config directory.

The hand-off notes file is created when the worker starts. The default
20-second window can be raised to 120 seconds with
`OFFLOAD_REVIEW_HANDOFF_SECONDS`. For each fact, record the command or method,
exact result, scope, and that this session ran it. Mark inferences and
unverified claims clearly. The drafter reads these notes and attributes the
results to the session; it must not claim it ran those commands itself.

Ticket filing needs no ambient setup beyond that file: `ticket-gate.sh`
sources it into the hook process, so `YOUTRACK_TOKEN` and `YOUTRACK_*`
reach the worker through the hook environment. The review chain reads the
credential directly from `~/.config/offload-workers/token`; `GITLAB_TOKEN` is
a legacy fallback.

## Environment

No personal paths or hosts live in this repo -- not even as defaults. A
default with your employer's GitLab in it ships your employer in every
checkout, so there are none: without configuration the worker fails loud
naming the missing vars. The hook sources `~/.config/offload-workers/env`
(mode 600, next to the token file) into workers, so interactive sessions
need no extra setup.

| var | does |
|---|---|
| `OFFLOAD_REPO` | repo under review (git history source) |
| `OFFLOAD_GITLAB_BASE_URL` | GitLab API base, e.g. `https://gitlab.example.com/api/v4` |
| `OFFLOAD_GITLAB_PROJECT` | project path, e.g. `group/sub/project` |
| `GITLAB_TOKEN` | legacy credential fallback; normally use `~/.config/offload-workers/token` |
| `YOUTRACK_TOKEN` | ticket credential (hook env via the sourced env file) |
| `YOUTRACK_URL` | YouTrack base URL (required, no default) |
| `YOUTRACK_PROJECT_ID` | numeric project id (required, unless per-cwd ids cover the session) |
| `YOUTRACK_PROJECT_MAP` | `prefix=id,...` table routing session cwd to project (longest prefix wins); global still wins when set. Find your numeric ids with `ai-youtrack list-projects` (the `id` field, e.g. `0-42`), then map each checkout root: `export YOUTRACK_PROJECT_MAP="$HOME/work/alpha=0-21,$HOME/work/beta=0-22"` |
| `AI_YOUTRACK_BIN` | ai-youtrack CLI override (optional) |
| `OFFLOAD_ALLOW_EXTERNAL_DATA` | must be `1` for the Headroom transport; allow it only if the selected model route may receive private session data |
| `OFFLOAD_REVIEW_HANDOFF_SECONDS` | review note-writing window (default 20 seconds, maximum 120) |
| `OFFLOAD_MODEL_TRANSPORT` | `headroom` (default, uses Headroom's route for the selected model) or `local-direct` (direct loopback model API, bypasses Headroom) |
| `OFFLOAD_MODEL_LOCAL_API` | `anthropic` (default, `/v1/messages`) or `openai` (`/v1/chat/completions`) for `local-direct` |
| `OFFLOAD_MODEL` | model id or Headroom alias; defaults to `claude-muse-spark-1.3`; set to the local server's model id for `local-direct` |
| `OFFLOAD_MODEL_HEADROOM_URL` | optional Headroom base URL (default `http://127.0.0.1:8787`) |
| `OFFLOAD_MODEL_LOCAL_BASE_URL` | local API base, such as `http://127.0.0.1:11434`; loopback only |
| `OFFLOAD_MODEL_LOCAL_AUTH_TOKEN` | optional local API token; defaults to the placeholder `ollama` |
| `OFFLOAD_MODEL_LOCAL_MAX_TOKENS` | local API output limit (default 8192) |
| `OFFLOAD_MODEL_TIMEOUT` | shared model call timeout in seconds; ticket calls are capped at 540 seconds |
| `OFFLOAD_REVIEW_TIMEOUT` / `OFFLOAD_REVIEW_BATCH` / `OFFLOAD_REVIEW_BATCH_TIMEOUT` | review drafter tuning |
| `OFFLOAD_TICKET_TIMEOUT` | ticket model/CLI call timeout in seconds (capped at 540; the hook supervisor stops at 600) |

Legacy `OFFLOAD_MODEL_BACKEND=proxy|local`, `OFFLOAD_MODEL_PROXY_URL`,
`SPARK_REVIEW_*`, `SPARK_DRAFT_*`, `SPARK_REPO`, and
`SPARK_GITLAB_*` settings remain accepted during migration.

Explicit writes:

```sh
bash contrib/offload-workers/review-post.sh --approve ~/.local/state/offload-workers/<session>.draft.json
bash contrib/offload-workers/review-post.sh --note <iid> <discussion-id> "One-line addendum"
bash contrib/offload-workers/review-post.sh --resolve <iid> <discussion-id>
```

## The three properties

**Idempotent.** Every new note carries `<!-- offload-worker:<hash> -->`, derived from
the thread id and the body text. A rerun re-reads the threads and skips
anything already carrying its marker, so a retry after a partial failure
finishes the job rather than double-posting half of it. Editing the draft text
changes the hash, so a revised reply posts instead of being mistaken for done.

**Verified.** Posting is not believing. After the writes it re-lists the
discussions from the server and confirms each marker is on the thread it was
meant for. This is the check that catches the `position: null` class of silent
failure, where the API returns 201, the note appears, and it is anchored to
nothing.

**Provable.** `<session>.proof.json` names every note id, thread, marker,
author and timestamp, and carries an `ok` flag that is false if anything failed
or went unverified. The caller never takes the worker's word for it.

## Credential isolation, and its limit

`install-credential.sh` moves the token from `.bashrc` to
`~/.config/offload-workers/token` (mode 600) and comments the export. After the
next restart of the reviewing model, its shell has no `GITLAB_TOKEN`, so
walking through the shape-based divert gate buys nothing — there is no
credential on the other side.

**This is not a permission boundary.** The poster runs as the same Unix user,
so anything that can run `cat` can still read the token file. Closing that
needs a second Unix user or a keyring with per-process ACLs, which needs root.
What this buys is the removal of *ambient* authority: the credential stops
arriving uninvited in every subprocess, and using it becomes a deliberate,
greppable act instead of an accident waiting to happen.

## Verified live

MR !591, thread `fdcea1af41c7`, note `89941` — posted, verified anchored
(`DiffNote` on `main_test.go:6`), proof written, rerun posted nothing.

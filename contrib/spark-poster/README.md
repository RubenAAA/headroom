# spark-poster

The posting and enforcement half of the review offload. The detection and
handoff half (session→transcript map, divert gate, drafter, Stop backstop)
lives in `contrib/claude/hooks/`; this is what was missing, and without it the
drafter could only ever end in `/tmp` as a dry run.

## Chain

```
review command runs (/gitlab-review or /fix-mr-comments → session armed)
  → user says "post the threads" (UserPromptSubmit divert)
  → hook spawns the worker: spark_draft.py <MR> <session> <mode>
  → evidence is pinned to the MR head (resolve_head fetches it; refusal
    beats a confident wrong draft)
  → draft written to ~/.local/state/spark-review/<session>.draft.json
    (flushed per thread-group; partial drafts carry head_sha + unverdicts)
  → hook posts it immediately: spark_post.py (idempotent; refuses if the
    MR head moved since the draft)
  → proof written; operator learns about it on the next prompt (one-shot
    ping, no file-touching chore)
```

No go-ahead step: the old `spark-goahead.sh` wait-for-a-file dance is
retired (the script stays for manual use). Approval is the instruction
itself; safety is idempotency + verification + proof, not a second chore.

Two survival properties, both learned from MR !597 (14 threads, ~65s each,
supervisor timeout fired one thread short):

- The drafter flushes the draft after every thread. A kill mid-loop leaves
  a partial draft (`"partial": true` + `unverdict_threads`) instead of
  nothing; the hook chains the listener for what exists rather than
  reporting failure. The poster only sends listed replies.
- The supervisor budget is 45 minutes; each worker call keeps its own
  shorter timeout. A worker call that dies empty-handed logs its rc and
  stderr tail instead of a bare NO VERDICT.

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

## State files (`~/.local/state/spark-review/`)

| file | means |
|---|---|
| `<session>.armed` | a review command really ran; content is the mode |
| `<session>.diverted` | a divert fired, worker started |
| `<session>.started` | supervisor pid; alive check before any respawn |
| `<session>.done` | worker finished (with proof, or with `.failed`) |
| `<session>.failed` | finished with no post; reason inside, first lines are the summary |
| `<session>.worker.log` | that run's transcript (draft + post) |
| `<session>.draft.json` | the replies, poster schema (+ `head_sha`, `partial`, `unverdict_threads`) |
| `<session>.proof.json` | verified note ids, markers, resolve results (`ok: false` + `refused` when the MR moved) |
| `<session>.reported` | the proof already pinged; the ping fires once |

## Pieces

| file | does |
|---|---|
| `gitlab_api.py` | transport. `rtk proxy curl`, JSON bodies via `--data-binary @file` |
| `spark_post.py` | the poster: idempotent, verified, provable |
| `spark-goahead.sh` | the listener: waits for approval, runs the poster once |
| `ticket_file.py` | the ticket worker: last-10-turns → headless file → proof |
| `install-credential.sh` | moves `GITLAB_TOKEN` out of the ambient environment |
| `read_threads.py`, `triage.py`, `since_review.py`, `thread_dossier.py` | read-only analysis helpers |

## Setup

One machine-side file holds the non-secret config; the hook sources it
into workers, so interactive sessions need no extra setup:

```sh
mkdir -p ~/.config/spark-poster
cat > ~/.config/spark-poster/env <<'EOF'
export SPARK_REPO=/path/to/repo-under-review
export SPARK_GITLAB_BASE_URL=https://gitlab.example.com/api/v4
export SPARK_GITLAB_PROJECT=group/project
export YOUTRACK_TOKEN=...
export YOUTRACK_URL=https://youtrack.example.com
export YOUTRACK_PROJECT_ID=0-00
# AI_YOUTRACK_BIN defaults to the ai-first-workspace checkout; set only to override.
EOF
chmod 600 ~/.config/spark-poster/env
```

The credential lives next to it: run `./install-credential.sh` to move
`GITLAB_TOKEN` out of the ambient environment into
`~/.config/spark-poster/token` (mode 600).

Ticket filing needs no ambient setup beyond that file: `ticket-gate.sh`
sources it into the hook process, so `YOUTRACK_TOKEN` and `YOUTRACK_*`
reach the worker through the hook environment. (The review chain instead
reads `~/.config/spark-poster/token` via `GITLAB_TOKEN`.)

## Environment

No personal paths or hosts live in this repo -- not even as defaults. A
default with your employer's GitLab in it ships your employer in every
checkout, so there are none: without configuration the worker fails loud
naming the missing vars. The hook sources `~/.config/spark-poster/env`
(mode 600, next to the token file) into workers, so interactive sessions
need no extra setup.

| var | does |
|---|---|
| `SPARK_REPO` | repo under review (git history source) |
| `SPARK_GITLAB_BASE_URL` | GitLab API base, e.g. `https://gitlab.example.com/api/v4` |
| `SPARK_GITLAB_PROJECT` | project path, e.g. `group/sub/project` |
| `GITLAB_TOKEN` | credential (or `~/.config/spark-poster/token`) |
| `YOUTRACK_TOKEN` | ticket credential (hook env via the sourced env file) |
| `YOUTRACK_URL` | YouTrack base URL (required, no default) |
| `YOUTRACK_PROJECT_ID` | numeric project id (required, no default) |
| `AI_YOUTRACK_BIN` | ai-youtrack CLI override (optional) |
| `SPARK_DRAFT_MODEL` / `SPARK_DRAFT_TIMEOUT` / `SPARK_DRAFT_BATCH` / `SPARK_DRAFT_BATCH_TIMEOUT` / `SPARK_DRAFT_BASE_URL` | drafter tuning, see `spark_draft.py` |

## The three properties

**Idempotent.** Every note carries `<!-- spark-poster:<hash> -->`, derived from
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
`~/.config/spark-poster/token` (mode 600) and comments the export. After the
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

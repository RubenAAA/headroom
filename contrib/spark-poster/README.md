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
  → draft written to ~/.local/state/spark-review/<session>.draft.json
  → hook chains spark-goahead.sh <session>, which waits for <session>.goahead
  → operator touches <session>.goahead          ← the human's deliberate act
  → spark_post.py replies on the threads
  → re-reads from the server, verifies every marker
  → writes <session>.proof.json                 ← the trust contract
```

The hook starts both halves -- draft and listener. Earlier the listener was
never launched by anything, so the go-ahead file was an approval nobody
heard; the one live posting (mr591) worked because the listener was started
by hand. A stale go-ahead is deleted when a fresh worker starts, so approval
can only ever apply to the draft it was given for.

## Modes

The scope depends on which command armed the session (recorded in
`<session>.armed`):

| mode | drafts replies to |
|---|---|
| `gitlab-review` | follow-ups on threads I opened |
| `fix-mr-comments` | reviewers' still-open threads on my own MR |

## State files (`~/.local/state/spark-review/`)

| file | means |
|---|---|
| `<session>.armed` | a review command really ran; content is the mode |
| `<session>.diverted` | a divert fired, worker started |
| `<session>.started` | supervisor pid; alive check before any respawn |
| `<session>.done` | worker finished (with draft, or with `.failed`) |
| `<session>.failed` | finished with no draft; reason inside, first lines are the summary |
| `<session>.worker.log` | that run's transcript |
| `<session>.draft.json` | the replies, poster schema |
| `<session>.goahead` | the approval; consumed once by the listener |
| `<session>.goahead.log` | listener + poster output |
| `<session>.proof.json` | verified note ids, markers, resolve results |

## Pieces

| file | does |
|---|---|
| `gitlab_api.py` | transport. `rtk proxy curl`, JSON bodies via `--data-binary @file` |
| `spark_post.py` | the poster: idempotent, verified, provable |
| `spark-goahead.sh` | the listener: waits for approval, runs the poster once |
| `install-credential.sh` | moves `GITLAB_TOKEN` out of the ambient environment |
| `read_threads.py`, `triage.py`, `since_review.py`, `thread_dossier.py` | read-only analysis helpers |

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

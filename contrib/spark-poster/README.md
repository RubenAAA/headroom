# spark-poster

The posting and enforcement half of the review offload. The detection and
handoff half (session→transcript map, divert gate, drafter, Stop backstop)
lives in `contrib/claude/hooks/`; this is what was missing, and without it the
drafter could only ever end in `/tmp` as a dry run.

## Chain

```
Opus reviews (reads only)
  → draft written to /tmp/opencode/poc2/<session>.draft.json
  → spark-goahead.sh <session> waits for <session>.goahead
  → operator touches <session>.goahead          ← the human's deliberate act
  → spark_post.py replies on the threads
  → re-reads from the server, verifies every marker
  → writes <session>.proof.json                 ← the trust contract
```

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

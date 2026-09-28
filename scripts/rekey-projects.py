#!/usr/bin/env python3
"""Move memories and ctx stores from path keys to repository keys.

A project used to be keyed on a path: memory partitions on
`sha256(repo root path)`, ctx stores (`sessions/<hash>.db`,
`content/<hash>.db`) on `sha256(exact cwd)`. Both now key a repository on its
`origin` URL, or its root path when it has none, so a moved checkout keeps
them and a subdirectory or worktree shares its repository's. What was written
under the old keys has to be moved once, or it stays behind under a key
nothing resolves to any more.

A key does not say which path it came from, so this hashes candidate paths
until one matches: every git repository under $HOME, and every path spelled by
a Claude Code project folder (`-home-ruben-ext-ai-lens`, which can be decoded
several ways, so all of them are tried). A path that no longer exists is looked
up by folder name among the repositories that do; two with the same name is
ambiguous and is left alone.

memory   Rewrites each partition's `user_id`. Dry by default; `--apply` backs
         the database up next to itself first. Safe with the proxy running,
         and safe to run again.

ctx      Renames each repository root's store files (with their `-wal` and
         `-shm`). Stores of subdirectories and worktrees stay where they are:
         merging sqlite files is not a rename, and `headroom_retrieve` still
         finds their hashes through the cold-tier sweep. Finding the paths takes
         minutes and renaming takes milliseconds, so they are two steps: `--plan
         FILE` works out the renames with the proxy running, `--apply-plan FILE`
         does them with it stopped (sqlite names its `-wal` after the path, so
         a file must not move while open).

Usage:
  scripts/rekey-projects.py memory [--apply]
  scripts/rekey-projects.py ctx --plan /tmp/ctx-plan.tsv
  scripts/rekey-projects.py ctx --apply-plan /tmp/ctx-plan.tsv
"""

import argparse
import glob
import hashlib
import itertools
import json
import os
import socket
import sqlite3
import sys
import time
from collections import defaultdict

DB = os.path.expanduser("~/.claude-work/context-mode/memory/memories.db")
CTX = os.path.expanduser("~/.claude-work/context-mode")
PROXY_PORT = 8787
SKIP_DIRS = {"node_modules", "target", ".venv", "venv", ".cache", ".git", "__pycache__"}


def sha16(text):
    return hashlib.sha256(text.encode()).hexdigest()[:16]


def path_key(path):
    return f"{sanitize(os.path.basename(path)) or 'project'}-{sha16(path)}"


def sanitize(value):
    out, last_dash = [], False
    for ch in value.strip():
        if ch.isascii() and (ch.isalnum() or ch in "._-"):
            out.append(ch)
            last_dash = False
        elif not last_dash:
            out.append("-")
            last_dash = True
    return "".join(out).strip("-._")[:64]


def repo_root(path):
    """Same walk as `ProjectResolver::repo_root`."""
    cursor = path
    while True:
        git = os.path.join(cursor, ".git")
        if os.path.isdir(git) and os.path.isfile(os.path.join(git, "HEAD")):
            return cursor
        if os.path.isfile(git):
            pointer = next(
                (l.split(":", 1)[1].strip() for l in open(git) if l.strip().startswith("gitdir:")),
                None,
            )
            if pointer is None:
                return cursor
            gitdir = pointer if pointer.startswith("/") else os.path.join(cursor, pointer)
            idx = gitdir.rfind("/.git/worktrees/")
            return gitdir[:idx] if idx >= 0 else cursor
        parent = os.path.dirname(cursor)
        if parent == cursor:
            return path
        cursor = parent


def origin_remote(root):
    """Same parse as `ProjectResolver::origin_remote`."""
    try:
        config = open(os.path.join(root, ".git", "config")).read()
    except OSError:
        return None
    in_origin, url = False, None
    for line in config.splitlines():
        line = line.strip()
        if line.startswith("["):
            in_origin = line == '[remote "origin"]'
        elif in_origin and "=" in line and line.split("=", 1)[0].strip() == "url":
            url = line.split("=", 1)[1].strip()
    if not url or url.startswith("."):
        return None
    rest = url.split("://", 1)[1] if "://" in url else url
    user, at, after = rest.partition("@")
    if at and "/" not in user:
        rest = after
    if "://" not in url:
        rest = rest.replace(":", "/", 1)
    rest = rest.rstrip("/")
    rest = rest[: -len(".git")] if rest.endswith(".git") else rest
    return rest.lower()


def identity(path):
    """Same as `ProjectResolver::project_identity`: (identity, display name)."""
    root = repo_root(os.path.realpath(path).rstrip("/") or "/")
    remote = origin_remote(root)
    if remote is None:
        return root, os.path.basename(root) or "root"
    return remote, remote.rsplit("/", 1)[-1]


def new_key(path):
    ident, name = identity(path)
    return f"{sanitize(name) or 'project'}-{sha16(ident)}"


def repos_under(top, depth=5):
    found = []
    for dirpath, dirnames, _ in os.walk(top):
        if dirpath.count("/") - top.count("/") >= depth:
            dirnames[:] = []
        if os.path.isdir(os.path.join(dirpath, ".git")):
            found.append(dirpath)
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
    return found


def slug_paths(slug):
    """Every path a Claude Code project slug could have been, with its prefixes."""
    tokens = slug.lstrip("-").split("-")
    for n in range(1, len(tokens) + 1):
        for seps in itertools.product("/-", repeat=n - 1):
            path = "/" + tokens[0] + "".join(s + t for s, t in zip(seps, tokens[1:n]))
            # `--` in a slug is a `/.` (a dot folder): `ruben--claude`.
            yield path.replace("//", "/.")


def locate(wanted, key_of):
    """Map each wanted key to the path `key_of` derives it from, where one does."""
    repos = repos_under(os.path.expanduser("~"))
    candidates = itertools.chain(
        repos,
        *(slug_paths(os.path.basename(d)) for d in glob.glob(os.path.expanduser("~/.claude*/projects/*"))),
    )
    found = {}
    for path in candidates:
        k = key_of(path)
        if k in wanted and k not in found:
            found[k] = path
    by_name = defaultdict(list)
    for r in repos:
        by_name[os.path.basename(r)].append(r)
    return found, by_name


def where_now(path, by_name):
    """Where the checkout at `path` lives today, or why that is unknown."""
    if os.path.isdir(path):
        return path, None
    same = by_name[os.path.basename(path)]
    if len(same) == 1:
        return same[0], None
    return None, f"{path} is gone; {len(same)} repos share its name"


def memory(args):
    con = sqlite3.connect(args.db)
    partitions = defaultdict(int)
    for (user_id,) in con.execute("SELECT user_id FROM memories"):
        partitions[user_id] += 1
    wanted = {u.split("::", 1)[1]: u for u in partitions if "::" in u}
    old_path, by_name = locate(wanted, path_key)

    moves = []
    for key, user_id in sorted(wanted.items(), key=lambda kv: -partitions[kv[1]]):
        base = user_id.split("::", 1)[0]
        n = partitions[user_id]
        path = old_path.get(key)
        if path is None:
            print(f"  keep   {key:48} n={n:<4} no path hashes to it")
            continue
        now, why = where_now(path, by_name)
        if now is None:
            print(f"  keep   {key:48} n={n:<4} {why}")
            continue
        target = new_key(now)
        if target == key:
            print(f"  same   {key:48} n={n:<4} {now}")
            continue
        merge = " (merges)" if f"{base}::{target}" in partitions else ""
        print(f"  move   {key:48} n={n:<4} -> {target}  {now}{merge}")
        moves.append((user_id, f"{base}::{target}"))

    print(f"{len(moves)} partitions to move, {sum(partitions[u] for u, _ in moves)} memories")
    if not args.apply or not moves:
        return

    backup = f"{args.db}.bak-{int(time.time())}"
    with sqlite3.connect(backup) as dst:
        con.backup(dst)
    print(f"backup: {backup}")
    with con:
        for old, new in moves:
            rows = con.execute("SELECT id, record FROM memories WHERE user_id = ?", (old,)).fetchall()
            for mid, record in rows:
                r = json.loads(record)
                r["user_id"] = new
                con.execute(
                    "UPDATE memories SET user_id = ?, record = ? WHERE id = ?",
                    (new, json.dumps(r, ensure_ascii=False), mid),
                )
    left = con.execute(
        f"SELECT count(*) FROM memories WHERE user_id IN ({','.join('?' * len(moves))})",
        [old for old, _ in moves],
    ).fetchone()[0]
    print(f"applied; {left} memories left on old keys")


def store_files(kind, stem):
    base = os.path.join(CTX, kind, f"{stem}.db")
    return [p for p in (base, base + "-wal", base + "-shm") if os.path.exists(p)]


def ctx_plan(args):
    stems = defaultdict(set)
    for kind in ("sessions", "content"):
        for f in glob.glob(os.path.join(CTX, kind, "*.db")):
            stem = os.path.basename(f)[:-3]
            # `<hash>__<hash>` files belong to the context-mode plugin, not the proxy.
            if len(stem) == 16 and all(c in "0123456789abcdef" for c in stem):
                stems[stem].add(kind)
    old_path, by_name = locate(set(stems), sha16)

    targets = defaultdict(list)
    for stem in sorted(stems):
        path = old_path.get(stem)
        if path is None:
            continue
        now, why = where_now(path, by_name)
        if now is None:
            print(f"  keep   {stem} {why}")
            continue
        real = os.path.realpath(now)
        if repo_root(real) != real:
            continue  # a subdirectory or worktree: stays, reachable by the sweep
        new = sha16(identity(now)[0])
        if new == stem:
            continue
        targets[new].append((stem, path, now))

    plan = []
    for new, sources in sorted(targets.items()):
        # Two old stores for one repository (it moved, or was cloned twice):
        # the one written last carries on; the other stays for the sweep.
        sources.sort(key=lambda s: -max(os.path.getmtime(f) for k in stems[s[0]] for f in store_files(k, s[0])))
        stem, path, now = sources[0]
        for other, other_path, _ in sources[1:]:
            print(f"  keep   {other} {other_path}: {stem} holds the newer store for {now}")
        for kind in sorted(stems[stem]):
            if store_files(kind, new):
                print(f"  keep   {kind}/{stem} {path}: {kind}/{new}.db already exists")
                continue
            size = sum(os.path.getsize(f) for f in store_files(kind, stem))
            print(f"  rename {kind}/{stem} -> {new}  {size / 2**20:9.1f} MiB  {path} -> {identity(now)[0]}")
            plan.append((kind, stem, new))

    print(f"{len(plan)} store files to rename")
    if args.plan:
        with open(args.plan, "w") as f:
            f.writelines(f"{k}\t{o}\t{n}\n" for k, o, n in plan)
        print(f"plan: {args.plan}")


def ctx_apply(args):
    with socket.socket() as s:
        if s.connect_ex(("127.0.0.1", PROXY_PORT)) == 0:
            sys.exit(f"the proxy is listening on {PROXY_PORT}; stop it first")
    held = set()
    for fd in glob.glob("/proc/[0-9]*/fd/*"):
        try:
            target = os.readlink(fd)
        except OSError:
            continue
        if target.startswith(CTX + "/sessions/") or target.startswith(CTX + "/content/"):
            held.add(target)
    if held:
        sys.exit(f"{len(held)} store files are open, e.g. {sorted(held)[0]}; stop what holds them first")

    plan = [line.rstrip("\n").split("\t") for line in open(args.apply_plan) if line.strip()]
    done = 0
    for kind, old, new in plan:
        sources = store_files(kind, old)
        if not sources or store_files(kind, new):
            print(f"  skip   {kind}/{old} -> {new}: {'source gone' if not sources else 'target exists'}")
            continue
        for src in sources:
            os.rename(src, src.replace(f"/{old}.db", f"/{new}.db"))
        done += 1
    print(f"renamed {done} of {len(plan)} store files")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="what", required=True)
    m = sub.add_parser("memory")
    m.add_argument("--db", default=DB)
    m.add_argument("--apply", action="store_true")
    c = sub.add_parser("ctx")
    g = c.add_mutually_exclusive_group()
    g.add_argument("--plan", help="write the renames here")
    g.add_argument("--apply-plan", help="do the renames in this plan; the proxy must be stopped")
    args = ap.parse_args()
    if args.what == "memory":
        memory(args)
    elif args.apply_plan:
        ctx_apply(args)
    else:
        ctx_plan(args)


if __name__ == "__main__":
    main()

#!/usr/bin/env bash
# Build-triggered garbage collection for Cargo's `target/` directory.
#
# Runs only when invoked (end of `make test` / `make build-proxy` /
# `make build-wheel`), never as a background timer. Modes:
#   --auto     gated run for build hooks: cheap no-op unless the last check
#              is older than GC_INTERVAL_HOURS. Always exits 0, never fails
#              the build. Missing cargo-sweep prints a hint and exits 0.
#   --force    `make gc`: ignore the interval gate and the CI guard, delete
#              for real. Exits non-zero on failure (explicit user intent).
#   --check    `make gc-check`: preview only (`cargo-sweep --dry-run` + wheel
#              list). Deletes nothing.
#
# What counts as "obviously not needed" (--auto and --force):
#   - artifacts from toolchains that are no longer installed (`--installed`).
#     Precise by definition; matters here because rust-toolchain.toml pins
#     the compiler, so every bump orphans the previous set.
#   - oldest artifacts while target/ is over GC_MAXSIZE (`--maxsize`).
#     Oldest-evicted-first and a no-op when under the cap, so this step is
#     inherently "only when needed".
#   - maturin wheels in target/wheels/ beyond GC_KEEP_WHEELS (keep-last-N).
#     cargo-sweep does not track these versioned outputs, so they are
#     pruned explicitly. Each wheel is small; the bound is what matters.
# Age-based sweeping (`--time GC_MAXAGE_DAYS`) runs only under --force /
# --check, never automatically: merely-old is not obviously-unneeded while
# disk usage is fine. The `--stamp`/`--file` workflow is deliberately not
# used: it deletes everything the last build did not touch, including
# week-old profiles the developer still wants.
#
# Tuning without editing (environment):
#   GC_MAXSIZE (default 15GiB), GC_MAXAGE_DAYS (default 90),
#   GC_KEEP_WHEELS (default 3), GC_INTERVAL_HOURS (default 24),
#   CARGO_TARGET_DIR (respected for stamp/lock/wheel paths, as with cargo).

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
STAMP="$TARGET_DIR/.gc-stamp"
LOCKDIR="$TARGET_DIR/.gc-lock"

GC_MAXSIZE="${GC_MAXSIZE:-15GiB}"
GC_MAXAGE_DAYS="${GC_MAXAGE_DAYS:-90}"
GC_KEEP_WHEELS="${GC_KEEP_WHEELS:-3}"
GC_INTERVAL_HOURS="${GC_INTERVAL_HOURS:-24}"

log() {
    printf '[cargo-gc] %s\n' "$*" >&2
}

MODE="auto"
for arg in "$@"; do
    case "$arg" in
        --auto) MODE="auto" ;;
        --force | --now) MODE="force" ;;
        --check | --dry-run) MODE="check" ;;
        -h | --help)
            sed -n '1,/^set -euo pipefail$/p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            log "unknown argument: $arg (see --help)"
            exit 2
            ;;
    esac
done

# Nothing built yet: nothing to clean. (Before lock acquisition, which
# needs the directory to exist for its mkdir.)
if [ ! -d "$TARGET_DIR" ]; then
    exit 0
fi

# CI runners are ephemeral and rust.yml manages its own cache
# (Swatinem/rust-cache); sweeping there only burns minutes. Explicit
# --force/--check still run on request.
if [ "$MODE" = "auto" ] && { [ -n "${CI:-}" ] || [ -n "${GITHUB_ACTIONS:-}" ]; }; then
    log "skipping under CI (use --force to override)"
    exit 0
fi

# Interval gate (--auto only): at most one real check per GC_INTERVAL_HOURS.
# Silent fast path so every build pays ~ms. --force/--check ignore the gate.
if [ "$MODE" = "auto" ] && [ -f "$STAMP" ]; then
    mins=$((GC_INTERVAL_HOURS * 60))
    if [ -z "$(find "$STAMP" -mmin +"$mins" 2>/dev/null)" ]; then
        exit 0
    fi
fi

# Mutual exclusion between concurrent gc runs. A build never holds this
# lock (only gc takes it, briefly), so contention just means another gc
# is already doing the work. The lock carries the holder's pid so a
# holder killed without running the trap (SIGKILL, `| head` SIGPIPE
# before PIPE was trapped, box crash) does not block future runs
# forever: a dead holder's lock is stolen, a live one's is respected.
release_lock() {
    rm -f "$LOCKDIR/pid" 2>/dev/null
    rmdir "$LOCKDIR" 2>/dev/null || true
}

acquire_lock() {
    if mkdir "$LOCKDIR" 2>/dev/null; then
        echo $$ >"$LOCKDIR/pid"
        return 0
    fi
    # Contention. A missing pid file may mean the holder is between
    # mkdir and pid-write; wait a beat and recheck before stealing.
    local pid=""
    [ -f "$LOCKDIR/pid" ] || sleep 1
    [ -f "$LOCKDIR/pid" ] && pid="$(cat "$LOCKDIR/pid" 2>/dev/null)"
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
        return 1 # live holder
    fi
    log "removing stale lock from dead process ${pid:-unknown}"
    rm -rf "$LOCKDIR" 2>/dev/null
    if mkdir "$LOCKDIR" 2>/dev/null; then
        echo $$ >"$LOCKDIR/pid"
        return 0
    fi
    return 1
}

if ! acquire_lock; then
    if [ "$MODE" = "auto" ]; then
        exit 0
    fi
    log "another cargo-gc run is active; aborting (its lock $LOCKDIR is live)"
    exit 1
fi
trap 'release_lock' EXIT INT TERM PIPE

touch_stamp() {
    touch "$STAMP" 2>/dev/null || log "warning: cannot write $STAMP"
}

if ! command -v cargo-sweep >/dev/null 2>&1; then
    log "cargo-sweep not found; skipping (install with: cargo install cargo-sweep)"
    # Touch the stamp in auto mode so the hint appears at most once per
    # interval rather than on every build. Explicit modes fail: the user
    # asked for a sweep/preview and cannot get one.
    if [ "$MODE" = "auto" ]; then
        touch_stamp
        exit 0
    fi
    exit 1
fi

cd "$ROOT"

FAILED=0
sweep() {
    # "$@" is one cargo-sweep criterion set. Check mode adds --dry-run.
    # Auto mode swallows failures (GC must never fail a build); force
    # mode records them and exits non-zero at the end.
    if [ "$MODE" = "check" ]; then
        cargo-sweep --dry-run "$@" || log "warning: cargo-sweep $* exited $?"
    elif [ "$MODE" = "auto" ]; then
        cargo-sweep "$@" || log "warning: cargo-sweep $* exited $?"
    else
        cargo-sweep "$@" || { log "error: cargo-sweep $* exited $?"; FAILED=1; }
    fi
}

prune_wheels() {
    local dir="$TARGET_DIR/wheels" f count=0
    [ -d "$dir" ] || return 0
    while IFS= read -r f; do
        count=$((count + 1))
        if [ "$count" -gt "$GC_KEEP_WHEELS" ]; then
            if [ "$MODE" = "check" ]; then
                printf '[cargo-gc] would remove %s\n' "$f" >&2
            else
                rm -f "$f" && printf '[cargo-gc] removed %s\n' "$f" >&2
            fi
        fi
    done < <(ls -t "$dir"/*.whl 2>/dev/null)
    return 0
}

if [ "$MODE" = "check" ]; then
    log "preview only; deleting nothing (maxsize=$GC_MAXSIZE maxage=${GC_MAXAGE_DAYS}d keep-wheels=$GC_KEEP_WHEELS)"
else
    log "sweeping $TARGET_DIR (maxsize=$GC_MAXSIZE keep-wheels=$GC_KEEP_WHEELS)"
fi

sweep --installed
sweep --maxsize "$GC_MAXSIZE"
if [ "$MODE" != "auto" ]; then
    sweep --time "$GC_MAXAGE_DAYS"
fi
prune_wheels

if [ "$MODE" = "check" ]; then
    exit 0
fi
touch_stamp
if [ "$MODE" = "force" ] && [ "$FAILED" -ne 0 ]; then
    exit 1
fi
exit 0

#!/usr/bin/env bash
# Ratchet: rustdoc links must resolve, every dependency must be used, and
# TOML files must be canonical.
#
# Why: doc comments carry intra-doc links ([`TurnClass::TtlExpiry`]) that
# rot silently — nothing builds them except `cargo doc`. Unused
# dependencies accumulate the same way: each is link-cost on all 80+
# integration binaries. TOML drift (unsorted deps, unformatted files)
# is review noise.
#
# All three legs fail hard. The doc leg documents private items and every
# feature, so links into private modules and into `ml`/`redis` code are
# checked too; the workspace allows rustdoc's private_intra_doc_links.
#
# Three checks, each skipped gracefully when its tool is absent (same
# policy as sccache: check, don't require):
#   1. cargo doc --workspace --no-deps --document-private-items
#      --all-features — always runs (ships with cargo).
#      -D warnings turns broken intra-doc links into failures.
#   2. cargo machete — needs `cargo install cargo-machete --locked`.
#   3. taplo fmt --check + cargo sort --check — need `cargo install
#      taplo-cli cargo-sort --locked`. Only files this push touches are
#      checked (see below), so unrelated drift never blocks a push.
#
# Usage: bash scripts/check-hygiene.sh [--base REF]
# With --base, TOML checks scope to files changed in REF...HEAD.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

fail=0

echo "── hygiene: cargo doc (intra-doc links)"
# RUSTDOCFLAGS persists -D warnings for the whole workspace build; a
# trailing -- -D warnings would only apply to the top-level crate.
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items --all-features || {
    echo "❌ hygiene: broken rustdoc links. Run 'cargo doc --workspace --no-deps --document-private-items --all-features'." >&2
    fail=1
}

if command -v cargo-machete >/dev/null 2>&1; then
    echo "── hygiene: cargo machete (unused deps)"
    # False positives machete cannot see, each confirmed by grep:
    #   cc (py): used in build.rs (`cc::Build`), which machete ignores
    #   proc-macro2 (itemspan): only `syn::spanned::Spanned` is used —
    #     no direct proc_macro2:: path, but syn re-exports it and the
    #     span-locations feature is load-bearing; keep, machete is wrong
    #   md-5 (core, proxy): imported as `md5`; ignored in each Cargo.toml.
    #     Plain mode on purpose: --with-metadata runs `cargo metadata`,
    #     which can rewrite Cargo.lock in the middle of a push.
    cargo machete || {
        echo "❌ hygiene: unused dependencies found." >&2
        fail=1
    }
else
    echo "── hygiene: cargo-machete not installed; skipping (cargo install cargo-machete --locked)"
fi

# TOML files changed in this push (or working tree without --base).
# Only touched files are checked, so unrelated drift never blocks a push.
# taplo formats whole files: a touched file fails only if THIS push's
# lines are unformatted (verified via taplo on a stash-scoped diff —
# see the leg below). cargo sort is grandfathered per file: a file
# already unsorted at HEAD stays that way; a file sorted at HEAD must
# stay sorted.
BASE=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --base) BASE="$2"; shift 2 ;;
        *) echo "usage: $0 [--base REF]" >&2; exit 2 ;;
    esac
done
if [[ -n "$BASE" ]]; then
    TOML_FILES="$(git diff --name-only "$BASE"...HEAD -- '*.toml' || true)"
else
    TOML_FILES="$(git diff --name-only HEAD -- '*.toml' 2>/dev/null || true)"
    UNTRACKED="$(git ls-files --others --exclude-standard -- '*.toml' || true)"
    TOML_FILES="$(printf '%s\n%s' "$TOML_FILES" "$UNTRACKED" | sort -u)"
fi

if [[ -n "${TOML_FILES// /}" ]]; then
    if command -v taplo >/dev/null 2>&1; then
        echo "── hygiene: taplo fmt --check (touched TOML)"
        # Whole-file check flags pre-existing drift (long feature lists
        # taplo wraps). Only added lines fail: format a copy, diff it
        # against the working file, and fail if any reformatted line is
        # one this push added (vs HEAD).
        for f in $TOML_FILES; do
            case "$f" in
                *.toml) : ;;
                *) continue ;;
            esac
            tmp_fmt="$(mktemp --suffix=.toml)"
            cp "$f" "$tmp_fmt"
            taplo fmt "$tmp_fmt" >/dev/null 2>&1 || { rm -f "$tmp_fmt"; continue; }
            # Lines taplo changed, intersected with lines the push added.
            if grep -vFxf "$f" "$tmp_fmt" | grep -q .; then
                # Same range the file list came from; `|| true` because a
                # file with no added lines makes grep exit 1 under pipefail.
                added="$(git diff "${BASE:+$BASE...}HEAD" -- "$f" | { grep -E '^\+' || true; } | { grep -vE '^\+\+\+' || true; } | sed 's/^+//' | sort -u)"
                if grep -vFxf "$f" "$tmp_fmt" | sort -u | grep -qFxf <(printf '%s\n' "$added") 2>/dev/null; then
                    echo "❌ hygiene: $f has unformatted lines added by this push. Run 'taplo fmt $f'." >&2
                    fail=1
                fi
            fi
            rm -f "$tmp_fmt"
        done
    else
        echo "── hygiene: taplo not installed; skipping (cargo install taplo-cli --locked)"
    fi
    if command -v cargo-sort >/dev/null 2>&1; then
        echo "── hygiene: cargo sort --check (touched TOML)"
        # Grandfathered per file: sorted at HEAD must stay sorted;
        # unsorted at HEAD is pre-existing drift, not this push's.
        for f in $TOML_FILES; do
            case "$f" in
                *.toml) : ;;
                *) continue ;;
            esac
            # Compare sort-status before/after this push's changes, via
            # temp copies (cargo sort takes file paths, not stdin).
            tmp_before="$(mktemp)"; tmp_after="$(mktemp)"
            git show "HEAD:$f" >"$tmp_before" 2>/dev/null || continue
            cp "$f" "$tmp_after"
            # cargo-sort keys off the file NAME — must end in .toml.
            before_toml="${tmp_before}.toml"; after_toml="${tmp_after}.toml"
            mv "$tmp_before" "$before_toml"; mv "$tmp_after" "$after_toml"
            BEFORE="$(cargo sort --check "$before_toml" >/dev/null 2>&1 && echo clean || echo dirty)"
            AFTER="$(cargo sort --check "$after_toml" >/dev/null 2>&1 && echo clean || echo dirty)"
            rm -f "$before_toml" "$after_toml"
            if [[ "$BEFORE" == "clean" && "$AFTER" == "dirty" ]]; then
                echo "❌ hygiene: $f was sorted and this push unsorted it. Run 'cargo sort -w $f'." >&2
                fail=1
            fi
        done
    else
        echo "── hygiene: cargo-sort not installed; skipping (cargo install cargo-sort --locked)"
    fi
else
    echo "── hygiene: no TOML files touched; skipping taplo/cargo-sort"
fi

exit "$fail"

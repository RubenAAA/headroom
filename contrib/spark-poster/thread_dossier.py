"""One dossier per review thread: what I asked, and what the code says now.

Pairs each thread's opening note with the current state of the lines it was
anchored to, plus whether that file moved in the commits that postdate the
review. That is the whole evidence base for "was this addressed" -- the
alternative, judging from the thread alone, cannot see a fix that landed as a
commit rather than a reply.
"""

import subprocess
import sys

import gitlab_api as gl

ME = gl.me()
REPO = ("/home/ruben/meta/ai-first-workspace/internal-b2b/"
        "b2b-technology/platform/b2b-amg")
HEAD = "FETCH_HEAD"
CONTEXT = 25


def git(*args):
    r = subprocess.run(["git", "-C", REPO, *args], capture_output=True, text=True)
    return r.stdout


def main():
    iid = sys.argv[1] if len(sys.argv) > 1 else "591"
    want = sys.argv[2] if len(sys.argv) > 2 else None

    ds = [d for d in gl.discussions(iid) if not d.get("individual_note")]
    mine = [
        d for d in ds
        if d.get("notes") and d["notes"][0].get("author", {}).get("username") == ME
    ]
    first_review = min(d["notes"][0]["created_at"] for d in mine)

    for d in mine:
        did = d["id"]
        if want and not did.startswith(want):
            continue
        n0 = d["notes"][0]
        pos = n0.get("position") or {}
        path = pos.get("new_path")
        line = pos.get("new_line") or pos.get("old_line")

        print("=" * 78)
        print(f"THREAD {did}")
        print(f"ANCHOR {path}:{line}")
        print("=" * 78)
        print("\n--- MY NOTE ---")
        print((n0.get("body") or "").strip())

        for n in d["notes"][1:]:
            print(f"\n--- REPLY by {n.get('author', {}).get('username')} "
                  f"at {(n.get('created_at') or '')[:19]} ---")
            print((n.get("body") or "").strip())

        if path:
            log = git("log", "--oneline", f"--since={first_review}",
                      f"{HEAD}", "--", path)
            print(f"\n--- COMMITS TOUCHING {path} SINCE REVIEW ---")
            print(log.strip() or "(none)")

            if line:
                lo, hi = max(1, int(line) - CONTEXT), int(line) + CONTEXT
                blob = git("show", f"{HEAD}:{path}")
                lines = blob.splitlines()
                print(f"\n--- {path} @ {HEAD} lines {lo}-{hi} ---")
                for i in range(lo - 1, min(hi, len(lines))):
                    print(f"{i + 1:6d}| {lines[i]}")
        print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

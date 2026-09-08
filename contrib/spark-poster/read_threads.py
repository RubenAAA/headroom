"""Dump an MR's review threads for analysis. Read-only, no writes anywhere.

Prints one block per unresolved-or-not discussion that started as a review
note, with every note in order, so the reader can judge whether a follow-up
actually answers the original.
"""

import sys

import gitlab_api as gl


def main():
    iid = sys.argv[1] if len(sys.argv) > 1 else "591"
    only = sys.argv[2] if len(sys.argv) > 2 else None

    mr = gl.merge_request(iid)
    if mr is None:
        print("could not fetch MR", iid)
        return 1
    print(f"MR !{iid}: {mr.get('title')}")
    print(f"state={mr.get('state')} author={mr.get('author', {}).get('username')}")
    print(f"branch {mr.get('source_branch')} -> {mr.get('target_branch')}")
    print(f"head_sha={(mr.get('diff_refs') or {}).get('head_sha')}")
    print()

    ds = gl.discussions(iid)
    threads = [d for d in ds if not d.get("individual_note")]
    print(f"{len(ds)} discussions, {len(threads)} threads\n")

    for d in threads:
        notes = d.get("notes") or []
        if not notes:
            continue
        first = notes[0]
        pos = first.get("position") or {}
        anchor = (
            f"{pos.get('new_path')}:{pos.get('new_line') or pos.get('old_line')}"
            if pos
            else "(unanchored)"
        )
        if only and only not in d.get("id", ""):
            continue
        print("=" * 78)
        print(f"discussion {d.get('id')}  {anchor}")
        print(f"resolved={first.get('resolved')} resolvable={first.get('resolvable')}")
        for i, n in enumerate(notes):
            who = n.get("author", {}).get("username")
            when = (n.get("created_at") or "")[:19]
            print(f"\n  --- note {i} by {who} at {when} (id={n.get('id')})")
            for line in (n.get("body") or "").splitlines():
                print(f"  {line}")
        print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

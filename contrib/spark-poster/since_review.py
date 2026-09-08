"""What landed on the branch after the review notes were written.

A thread going unanswered in the UI says nothing about whether the code moved;
authors here answer in commits. This lists the commits that postdate the
earliest review note, and the files each touched, so every thread can be
checked against the code as it stands rather than as it was reviewed.
"""

import sys

import gitlab_api as gl

ME = gl.me()


def main():
    iid = sys.argv[1] if len(sys.argv) > 1 else "591"

    ds = [d for d in gl.discussions(iid) if not d.get("individual_note")]
    mine = [
        d["notes"][0]["created_at"]
        for d in ds
        if d.get("notes") and d["notes"][0].get("author", {}).get("username") == ME
    ]
    if not mine:
        print("no notes by", ME)
        return 1
    first_review = min(mine)
    print(f"earliest review note: {first_review}\n")

    status, commits = gl.call(
        "GET", f"/projects/{gl.PROJ_ENC}/merge_requests/{iid}/commits?per_page=100"
    )
    if status != 200 or not isinstance(commits, list):
        print("could not list commits:", status)
        return 1

    after = [c for c in commits if (c.get("created_at") or "") > first_review]
    print(f"{len(commits)} commits on the MR, {len(after)} after the review\n")
    for c in sorted(after, key=lambda c: c.get("created_at") or ""):
        print(f"{c['short_id']} {(c.get('created_at') or '')[:19]} "
              f"{c.get('author_name')}")
        print(f"  {(c.get('title') or '')[:100]}")
        st, diff = gl.call(
            "GET", f"/projects/{gl.PROJ_ENC}/repository/commits/{c['id']}/diff"
        )
        if st == 200 and isinstance(diff, list):
            for f in diff:
                print(f"    {f.get('new_path')}")
        print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

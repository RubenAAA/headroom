"""Which of my threads did the author answer, and where does each one stand.

System notes do not count as answers. GitLab posts "changed this line in
version N of the diff" into a thread under the author's name, with
`system: true` -- it reads exactly like a reply in the API and means only that
the line moved. Counting those as answers reported two of my threads as
addressed when nobody had written a word on either.

Splits the MR's threads into the three piles that matter for a follow-up pass:
answered (someone replied after my note), silent (nobody did), and resolved
(closed out). Prints anchors and the reply text so the judgement can be made
against what was actually said rather than against a count.
"""

import sys

import gitlab_api as gl

ME = gl.me()


def main():
    iid = sys.argv[1] if len(sys.argv) > 1 else "591"
    ds = [d for d in gl.discussions(iid) if not d.get("individual_note")]

    answered, silent, resolved = [], [], []
    for d in ds:
        notes = d.get("notes") or []
        if not notes or notes[0].get("author", {}).get("username") != ME:
            continue
        replies = [
            n for n in notes[1:]
            if not n.get("system")
            and n.get("author", {}).get("username") != ME
        ]
        if notes[0].get("resolved"):
            resolved.append((d, replies))
        elif replies:
            answered.append((d, replies))
        else:
            silent.append((d, replies))

    def anchor(d):
        pos = (d["notes"][0].get("position") or {})
        return f"{pos.get('new_path')}:{pos.get('new_line') or pos.get('old_line')}"

    def headline(d):
        body = (d["notes"][0].get("body") or "").strip().splitlines()
        return body[0][:110] if body else ""

    print(f"threads opened by {ME}: "
          f"{len(answered)} answered, {len(silent)} silent, {len(resolved)} resolved\n")

    for label, pile in (("ANSWERED", answered), ("SILENT", silent), ("RESOLVED", resolved)):
        print("#" * 78)
        print(f"# {label} ({len(pile)})")
        print("#" * 78)
        for d, replies in pile:
            print(f"\n[{d['id'][:12]}] {anchor(d)}")
            print(f"  mine: {headline(d)}")
            for r in replies:
                who = r.get("author", {}).get("username")
                when = (r.get("created_at") or "")[:19]
                print(f"  reply by {who} at {when}:")
                for line in (r.get("body") or "").strip().splitlines():
                    print(f"    {line}")
        print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

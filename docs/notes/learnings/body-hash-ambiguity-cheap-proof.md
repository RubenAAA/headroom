# Learning: the footnote field carried the proof, not the designed comparator

- **Source:** `docs/notes/proxy-experiments-2026-08.md` §11 design notes
- **Claim:** `prefix_body` (fixed-depth hash) was built as the comparator and
  `prefix_stable_msgs` added as a footnote — the footnote proved the merged-key
  case, because forked streams agree on the body hash (shared opener) while
  counts can't lie. Also: hashing everything-but-the-tail decides nothing (it
  grows every turn, so pairs never agree).
- **Rule:** prefer a cheap field that cannot be argued with over a carefully
  reasoned one.

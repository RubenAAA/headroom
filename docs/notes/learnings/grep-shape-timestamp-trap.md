# Learning: grep-shaped parsing eats timestamps

- **Status:** fixed via `ideas/implemented/search-verbatim-fix.md`
- **Source:** `docs/notes/proxy-experiments-2026-08.md` §16
- **Claim:** `parse_match_line` reads `<path><sep><digits><sep><body>` and an
  ISO timestamp fits exactly (minute lands in the `u64` line slot, unpadded on
  render). Clincher: `22:32:11` survived *because 32 needs no padding* — a
  generic stripper couldn't spare it. Lesson for every lenient parser on this
  path: select with parsed fields, emit source bytes.

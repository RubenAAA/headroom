# Idea: per-provider kompress hard-off

- **Status:** open (trivial, hardening only)
- **Source:** 2026-09-18 lossless audit. Global `--enable-kompress false` is
  live (`contrib/headroom-flags.sh:612-613`), but
  `--disable-kompress-anthropic/openai` are both `false`
  (`config.rs:1688-1693`, `flags.sh:615-616`), leaving the per-provider kill
  switches unset.
- **Value:** none in bytes — zero behavior change while the global flag is
  off. Pure defense-in-depth: a future global flip can't silently start ML
  rewriting on one provider while the other stays pinned.
- **Next:** set both `true`, confirm byte-identical output on a canary window
  (expect zero diffs by construction), close.

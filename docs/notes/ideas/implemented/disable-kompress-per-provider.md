# Idea: per-provider kompress hard-off

- **Status:** done 2026-09-28. The premise was wrong: all five kompress
  kill switches were parsed and never read, so setting them `true` would
  have protected nothing. Now wired: `--disable-kompress` overrides
  `--enable-kompress` (default changed `true` → `false` so
  `--enable-kompress true` alone still works), and
  `--disable-kompress-anthropic/openai` ride
  `DispatchConfig::disable_kompress`; a switched-off provider bypasses the
  dispatch memo and reports `declined_by: kompress_disabled`. Off-only: unlike
  Python, they cannot enable Kompress for one provider.
  `--disable-kompress-fallback` has nothing to wire (the dispatcher has no
  fallback; plain text already passes through). `--force-kompress-all` is
  unimplemented; it belongs to `kompress-enable-ab.md`. Flags file left at
  `--disable-kompress true` and both per-provider switches `false`.
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

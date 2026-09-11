# Idea: wire or remove the dead crush flags

- **Status:** open
- **Source:** `docs/notes/recache-classification.md` (2026-08-23)
- **Summary:** `--min-tokens-to-crush` (default 200) and `--max-items-after-crush` (default 15) are declared, copied into runtime `Config`, and never read — changing them does nothing to forwarded requests.
- **Next:** wire `SmartCrusher` construction to the CLI values, or delete the flags.
- **Update 2026-09-11:** strict-win investigation found neither option
  qualifies (delete breaks CLI parsing for existing flag files; wiring
  changes behavior + needs a `OnceLock` redesign). Docs-only mitigation
  shipped: both flags annotated NO-OP in `config.rs` help text. The
  wire-or-delete decision stays open.


## Detail

*moved from `docs/notes/recache-classification.md`*

**Both crush flags are dead.** `--min-tokens-to-crush` (`config.rs:1510`,
default 200) and `--max-items-after-crush` (`config.rs:1518`, default 15) are
declared, copied into the runtime `Config`, and never read by the request path.
The live `SmartCrusher` is built once from `SmartCrusherConfig::default()` at
`live_zone.rs:607`, so the CLI values cannot reach it. Same three-layer pattern
as the previously documented dead flag. Changing either from the command line
has no effect on forwarded requests.

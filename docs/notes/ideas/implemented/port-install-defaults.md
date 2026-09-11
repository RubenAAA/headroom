# Idea: port install-mode defaults

- **Status:** shipped 2026-09-11
- **Source:** `docs/notes/upstream-port-backlog.md` group B
- **Summary:** `cli/install.py` (+493/-47, 11-commit scope) — cache-mode
  default matching `headroom proxy`, Windows `CREATE_NO_WINDOW` fix.
- **Shipped:** `--mode` default `token`→`cache` (+ `HEADROOM_MODE`, `for_test`
  helper) in `crates/headroom-proxy/src/config.rs`; Windows
  `CREATE_NO_WINDOW` (`0x0800_0000`) in `cursor/agent.rs` +
  `bin/headroom_cli/tools.rs`; 4 regression tests; `docs/flags.md` regen.
  (Windows path typechecked via scratch crate only — not runtime-verified.)

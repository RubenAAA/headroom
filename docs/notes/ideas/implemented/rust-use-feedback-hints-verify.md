# Idea: verify use_feedback_hints=False is honored in the Rust crusher

- **Status:** done 2026-09-11 (fixture `use_feedback_hints_false_matches_true_on_triggering_fixture` in `crusher.rs` tests, passing)
- **Close-out:** feedback is stubbed at Stage 3c.1 (never produces hints), so
  the flag is behavior-preserving today — true/false crush byte-identical on a
  30-identical-dicts triggering fixture. Re-check when the Stage 3c.2 feedback
  integration lands; divergence there is expected, not a regression.
- **Source:** `docs/notes/rust-dev.md` (watch list)
- **Summary:** `SmartCrusherConfig.use_feedback_hints` forwards to Rust, but
  honoring inside the crusher hasn't been verified against a parity fixture
  for the disabled path.
- **Next:** parity fixture for the disabled path; close or file the gap.

# Implemented: Bedrock per-user ARN pinning (was already there)

- **Status:** done — verification item, not a build (entry was wrong; same
  failure mode as §1's correction)
- **Source:** `docs/notes/rust-parity-gaps.md` §4
- **Summary:** `HEADROOM_BEDROCK_MODEL_MAP` parsed in `bedrock/vendor.rs:39`,
  applied on both invoke paths with logging. Lesson: grep for the feature
  marker string, don't trust the entry.


## Detail

*moved from `docs/notes/rust-parity-gaps.md`*

## 4. Bedrock per-user application-inference-profile ARN pinning — DONE

`HEADROOM_BEDROCK_MODEL_MAP` is parsed and applied in
`crates/headroom-proxy/src/bedrock/vendor.rs:39`, and both call paths use it:
`invoke.rs:211` and `invoke_streaming.rs:188`, each logging when an override
lands. An earlier pass of this doc listed the feature as missing. That was
wrong — same failure mode as the item 1 correction, so re-verify with a grep
for the feature marker string rather than trusting the entry.

<details><summary>Original scoping notes</summary>

- Python: second half of `33c7f6cd` (#1795) — `HEADROOM_BEDROCK_MODEL_MAP`
  env-driven override letting operators pin specific per-user Bedrock
  application-inference-profile ARNs (for cost attribution).
- Rust: the *other* half of this commit (the `global.*` inference-profile
  prefix normalization) is already correctly ported and tested
  (`crates/headroom-proxy/src/bedrock/vendor.rs:16`, `GEO_PREFIXES`). Only
  the `HEADROOM_BEDROCK_MODEL_MAP` per-user ARN override is missing — this
  is a genuinely new operator-facing feature, not a bug fix, so lower
  priority than the others above.

</details>

# Idea: whole-body fail-open check on Responses write-back

- **Status:** open (hardening, needs a trigger case first)
- **Source:** upstream `be00a798` (gateway Responses shape, Sept 2026
  session). Its `apply_view` is positional (view message *i* owns slot
  *i*) and returns the body **unchanged** when the pipeline hands back
  a list that doesn't line up with the slots — compression is an
  optimisation, a mangled transcript is a broken request. Our live
  path (`live_zone_responses.rs`) fails open per block (only the
  failing block reverts); there is no whole-body alignment check
  after write-back.
- **Value:** defense against a class of silent transcript corruption
  (`reasoning.encrypted_content` rewritten, `call_id` pairing
  severed) that presents as valid requests the provider rejects —
  or worse, accepts with the reasoning chain cut.
- **Next:** don't build it speculatively. If a mangled Codex
  transcript ever shows up, check first whether per-block revert
  already contained it; only then add a post-write-back alignment
  assertion (slot count / item identity) that falls back to the
  original body. Close with the incident (or lack of one) recorded.

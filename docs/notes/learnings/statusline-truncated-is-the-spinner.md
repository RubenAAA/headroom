# Learning: `[truncated: ...]` on an agent's status line was the spinner, not the agent

- **Source:** `~/headroom-proxy.log` and `.log.1`, read 2026-10-02. A user
  asked which model failed when two `spark` rows read
  `◯ spark  [truncated: the connection to the API dropped mid-response]`.
- **Claim:** the text in an agent's status-line row is the spinner sidecar's
  description of that agent, not words the agent wrote. All 35
  `stream_tail_synthesised` on 2026-10-02 were sidecar calls to
  `space-bunny-free` (506 attempts, 7%); `.log.1` had 415 of 10,657 (4%).
  None was a Spark turn.
- **Cause:** the routed chat sidecar relayed Zen's stream as it came, under
  a 15 s `--sidecar-route-timeout` that covers the body too. A slow stream
  died after its first bytes had gone out, too late for the `Working`
  fallback, so `finish_on_drop` closed it with the truncation marker.
  Most died at 15.5 s.
- **Fixed 2026-10-02:** `routed/sidecar.rs` holds the stream until it reaches
  `message_stop` with text, and falls back to `--sidecar-local-answer`
  otherwise (`sidecar_routed_fallback`, `reason=incomplete_stream`). Test:
  `a_chat_route_that_stalls_mid_reply_falls_back_to_the_fixed_line`.
- **Why the first answer was wrong:** the marker's source was found by grep
  and the component was guessed from code and comments. The flag file and the
  `try_routed_sidecar` doc both said every failure falls back; that held only
  before the first byte. The earlier note
  `spark-stream-drops-and-503s-2026-09-30.md` also said sidecar drops are
  seen by no worker, which was wrong for the status line.
- **Check before naming a component:** join the drop to its request.

  ```bash
  python3 - ~/headroom-proxy.log <<'EOF'
  import sys, json, collections
  ev = collections.defaultdict(set)
  for l in open(sys.argv[1], errors="ignore"):
      i = l.find("{")
      try: f = json.loads(l[i:])["fields"]
      except Exception: continue
      if f.get("request_id"): ev[f["request_id"]].add(f.get("event"))
  drops = [r for r, e in ev.items() if "stream_tail_synthesised" in e]
  print(collections.Counter("sidecar" if "sidecar_detected" in ev[r] else "turn" for r in drops))
  EOF
  ```

# quality-replay

Does what the proxy forwards change what the model does next? Replays
captured turns on Sonnet, straight to Anthropic with this machine's Claude
Code login: the client's original body twice and the forwarded body once.
A judge (the same model, given the full original conversation) then picks
the better next step, original vs forwarded, with original vs original as
the control. 50% wins for the forwarded arm means no quality change.

Keep output outside the repo; the bodies are real sessions.

```bash
cd ~/.cache/hr-retention-check/quality-replay
python3 ~/headroom/scripts/quality-replay/select.py picks.jsonl 60 ~/headroom-capture-*   # free
python3 ~/headroom/scripts/quality-replay/replay.py picks.jsonl run3 10   # stops near 10% of a 5h window
python3 ~/headroom/scripts/quality-replay/score.py run3
```

A capture's `out/` holds what the proxy forwarded the day it was taken, so
replaying it judges that day's flags. To judge today's, re-forward the client
bodies through the installed proxy first (offline, loopback only, about an
hour for the four captures) and select from the result:

```bash
systemd-run --user --scope -p MemoryMax=4G -p MemorySwapMax=0 \
  python3 ~/headroom/scripts/quality-replay/reforward.py reforward-DATE ~/headroom-capture-*
python3 ~/headroom/scripts/quality-replay/select.py picks-DATE.jsonl 60 reforward-DATE
```

Keep the memory cap: earlyoom here kills `headroom-proxy` first, and a run
that grows takes the live proxy with it.

`replay.py` judges each pair right after replaying it, while the prefix is
cached; `judge.py RUN_DIR` judges pairs a run left unjudged. Watch the live
meter (`unified_utilization_sample` in the proxy log) between runs.

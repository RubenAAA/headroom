# Streams that die mid-turn: TLS record corruption under WSL2

Measured 2026-08-20 on `local/npu-integrated`, WSL2 kernel 6.6.87.2, proxy on
`127.0.0.1:8787` against `api.anthropic.com`.

## What it looks like

Claude Code turns end early. Before the stream finisher existed the client
reported `API Error` and lost the turn outright. With the finisher in place the
turn ends cleanly and carries `[truncated: the connection to the API dropped
mid-response]`, which is survivable but still loses whatever the model was
saying.

The proxy logs the drop as an ordinary body error:

```
upstream stream error mid-response   error="error decoding response body"
```

`Display` on a reqwest error stops there, which is why this went unexplained for
so long. The cause chain says what actually happened:

```
reqwest::Error { kind: Decode, source: reqwest::Error { kind: Body,
  source: hyper::Error(Body, Error { kind: Io(Custom { kind: InvalidData,
    error: "received fatal alert: BadRecordMac" }) }) } }
```

## What BadRecordMac means

A TLS record arrived whose authentication tag did not verify. The bytes were
corrupted somewhere between the server and our TLS stack. TLS cannot repair a
record and cannot trust the rest of the session, so it sends a fatal alert and
tears the connection down.

Everything riding on that connection dies at the same instant. The proxy's
upstream client speaks HTTP/2 (`proxy.rs:245`; `http1_only` is set only when an
HTTP proxy is configured), so parallel requests to Anthropic are multiplexed
over one connection. Four in-flight turns, one bad record, four dead streams —
which is exactly the pattern in the log, drops arriving in twos and threes with
identical timestamps.

It also explains why an immediate retry could fail to send: the connection pool
handed back the connection that had just been torn down.

## Why WSL2

Corruption on the receive path, on a virtual NIC, under sustained load. The
interface runs NAT networking at MTU 1420 with every offload enabled:

```
rx-checksumming: on   tcp-segmentation-offload: on
tx-checksumming: on   generic-segmentation-offload: on
                      generic-receive-offload: on
```

Two of those matter here. `generic-receive-offload` merges inbound packets
before the kernel sees them and is a known source of the corruption itself.
`rx-checksumming` is what lets the damage through: the NIC asserts the TCP
checksum is valid, the kernel skips verifying it, and corrupt bytes travel up
to TLS. Verify in the kernel instead and the same corruption becomes a
discarded segment and a retransmit that nobody notices.

The load correlation follows from this. Light traffic never triggers it; bursts
of parallel subagent turns trigger it every minute or two.

## The damage it does to agent workflows

This is not only lost text. Of eight truncated turns in one session, here is
what was open when each died:

```
3 x  tool block (Bash) never closed
2 x  tool block (Agent) never closed          <- spawning a subagent
2 x  tool block (SendMessage) never closed    <- messaging a teammate
```

A tool call arrives as a stream of JSON fragments. Cut one part-way and the
fragment is either unparseable or, worse, parseable and wrong. So the finisher
discards it and downgrades `stop_reason` to `end_turn`, and the turn ends
looking well-formed, having done nothing.

From the client's side that reads as an agent which finished and reported
nothing. "Idle with no report" is what a dropped `SendMessage` looks like from
outside. "Came back before its own sub-checks had reported" is a dropped
`Agent` call. The orchestration was never at fault.

## The fix

```
sudo ethtool -K eth0 gro off rx off tso off gso off
```

`gro` and `rx` do the work. `tso` and `gso` are transmit-side, included because
they cost nothing to disable. The cost is CPU: segmentation and checksums move
into the kernel, so throughput on bulk transfers drops a little. On API traffic
it does not register.

Two caveats. It applies to every process in the WSL instance the moment it
runs, and flipping offload can bounce the NIC queues, so run it between turns
rather than mid-stream. And if the bytes are mangled before the checksum is
computed rather than after, no checksum can catch them — in that case the
next step is `networkingMode=mirrored` in `.wslconfig`, which skips the NAT
path entirely.

It does not survive `wsl --shutdown` on its own, so it is now applied as a
`[boot] command` in `/etc/wsl.conf`:

```
command = "echo always > /sys/kernel/mm/transparent_hugepage/enabled; /usr/sbin/ethtool -K eth0 gro off rx off tso off gso off"
```

so it re-applies on every WSL boot without a manual step.

## What the proxy does in the meantime

None of this is the proxy's fault, but the proxy is where the damage lands, and
three changes made the failure survivable rather than fatal.

`sse/stream_retry.rs` holds the opening bytes of every stream. While the held
buffer is under `--retry-stream-hold-bytes` nothing has reached the client, so a
drop there discards the buffer and re-issues the request, and the client sees
one clean stream. Raised from 2048 to 8192 after a drop at 2279 bytes truncated
a turn. Observed drop points since: 641, 1129, 2122, 2279 bytes.

The same file used to give up when a re-issued request failed to reach the
network — one failed send ended the turn with two attempts still in the budget,
which is precisely what happened when the pool returned a dead connection. It
now keeps asking until a body is in hand or the budget genuinely runs out.

`sse/stream_finisher.rs` handles drops past the hold, where re-issuing would
splice two generations together. It closes the open block, marks the reply
truncated and ends the turn, so the session survives.

## Reading the logs

```
dropped before the client was committed   a drop the hold caught; retried, silent
stream_tail_synthesised                   a drop past the hold; the turn was truncated
tool_call_defect                          what was open when the stream died
stop_reason_downgraded                    a tool call discarded, stop reason corrected
```

The first going up while the second stays at zero is the healthy shape. The
second appearing at all means drops are landing past the hold and the number
wants raising — or, given the fix is now applied at boot, that the offload
settings did not stick (check `ethtool -k eth0`) or the corruption is
happening before the checksum, which points at `networkingMode=mirrored`.

## Reproduced away from Anthropic — 2026-08-24

The offload settings did stick. `ethtool -k eth0` reads `gro`, `rx`, `tso` and
`gso` all off, applied by the `[boot] command`. BadRecordMac carried on
regardless: 34 events in a day, 16 of them past the hold.

So the second branch is the live one, and `scripts/tls-corruption-repro.sh`
tests it. Concurrent 300KB uploads, eight of them multiplexed over one HTTP/2
connection, aimed at `speed.cloudflare.com/__up`:

```
batches 546, requests 4368
bad_record_mac / decrypt errors : 15
curl: (56) OpenSSL SSL_read: OpenSSL/3.0.13:
      error:0A0003FC:SSL routines::sslv3 alert bad record mac
```

0.34% against the proxy's 0.85% over the same day — the same order, at an
endpoint that is not Anthropic, through OpenSSL 3.0.13 rather than the proxy's
rustls. That clears the proxy, clears rustls 0.23.41 and h2 0.4.16, and clears
the route to the provider. What is left is this instance, on the way out, which
is the case for `networkingMode=mirrored`.

One caveat kept on the record: both endpoints sit behind Cloudflare, so a fault
at that edge is not formally excluded. Two unrelated TLS stacks failing the
same way makes the local explanation much the stronger one.

### The drops are per connection, not per request

Worth knowing before reading any burst as an outage. All 34 events fall in 12
distinct 100ms buckets, and one bucket killed eight requests in the same
millisecond:

| time | requests killed together |
|---|---|
| 20:04:04 | 8 |
| 20:02:38 | 6 |
| 20:06:13 | 4 |
| 20:05:50 | 3 |

One connection's TLS state breaks and every stream multiplexed on it dies at
once. The 20 "drops" between 20:02 and 20:06 were four connection failures.
A reproducer that opened one connection per request would not show this, which
is why the script uses `curl --parallel` in a single process.

### It tracks load

Median throughput in a minute carrying a BadRecordMac is 41.5 req/min against
9.0 overall. Not deterministic, though: 20:03 ran 171 requests clean, and so
did 13:16 (135) and 11:56-11:58 (95-112). Close to necessary, plainly not
sufficient.

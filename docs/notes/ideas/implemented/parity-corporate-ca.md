# Implemented: corporate CA trust on every outbound client

- **Status:** done 2026-08-28 + source-wiring test
- **Source:** `docs/notes/rust-parity-gaps.md` §9.2 (upstream `36cc8001`)
- **Summary:** `ssl_context.rs` TLS setup had zero callers while seven clients
  built their own TLS (all failing behind inspecting proxies). All routed
  through `configure_client_tls`; `SSL_CERT_FILE`/`REQUESTS_CA_BUNDLE`
  replace, `NODE_EXTRA_CA_CERTS` adds. Test rejects future direct
  `reqwest::Client::builder()` outside `ssl_context.rs`.


## Detail

*moved from `docs/notes/rust-parity-gaps.md`*

### 9.2 Corporate CA trust — DONE (2026-08-28)

All production async and blocking reqwest builders now start from the
TLS-aware constructors in `ssl_context.rs`: the main proxy upstream, ctx
fetch, Copilot device auth, CLI tools download, CLI client, and subscription
tracker. `SSL_CERT_FILE` / `REQUESTS_CA_BUNDLE` now have actual replacement
semantics (`tls_built_in_root_certs(false)`); `NODE_EXTRA_CA_CERTS` remains
additive. A source-wiring integration test rejects any future direct reqwest
builder outside `ssl_context.rs`.

<details><summary>Original scoping notes</summary>

Upstream `36cc8001` made Copilot's token refresh honor a corporate CA bundle.
The Rust side has the harder half already: `ssl_context.rs:126`
`configure_client_tls` reads `SSL_CERT_FILE` / `REQUESTS_CA_BUNDLE` as
replacements and `NODE_EXTRA_CA_CERTS` as an additive bundle, and handles
`HEADROOM_TLS_STRICT`. It has **zero callers**:

```
grep -rn configure_client_tls crates/ --include=*.rs
```

returns only the definition. Every outbound client builds its own TLS:

| Client | Site |
|---|---|
| main proxy upstream | `proxy.rs:261` |
| a second proxy client | `proxy.rs:11221` |
| ctx fetch | `ctx/fetch.rs:205` |
| Copilot device auth | `bin/headroom_cli/copilot_auth.rs:70` |
| CLI tools fetch | `bin/headroom_cli/tools.rs:338` |
| CLI | `bin/headroom_cli.rs:365` |
| subscription tracker | `subscription.rs:29` |

Behind a TLS-inspecting corporate proxy every one of these fails, and the
symptom is a certificate error from whichever ran first. The work is small —
route each builder through `configure_client_tls` — but it is seven sites, and
each needs a look at whether it should trust the operator's bundle. The three
CLI ones talk to GitHub and the tools registry, so probably yes. Worth a test
that fails if a new `reqwest::Client::builder()` appears without it; otherwise
the eighth site will skip it too.

</details>

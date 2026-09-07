## Memory (Headroom)

Two separate systems. Conflating them is the usual mistake.

- **Long-term memory** — `memory_search`, `memory_save`, `memory_update`, `memory_delete`, `memory_list`. Durable facts that outlive the session.
- **Context offload** — `headroom_retrieve(hash=...)`, or `headroom ctx search|get`. Recovers tool output that was truncated *in this session*. Nothing to do with long-term memory.

### Search before you work, not just before you save

**Run `memory_search` at the start of any task involving infrastructure, hosts, credentials, DB topology, provider quirks, or past debugging.** This is the rule that actually pays. A session once spent a whole review rediscovering where `raw_payloads` lives; the answer was already stored four times over, the oldest a month old. Cost: wrong numbers reported to the user, then retracted.

Search first also prevents duplicates. If a memory on the topic exists, `memory_update` it — don't `memory_save` a fifth copy.

### Never claim a save you didn't make

Say "saved" only after the tool call returns an ID. Writing "Saved to memory." in prose does nothing — it is narration, not a side effect, and it produces a confident false statement the user has to catch. If you meant to save, call the tool; if the call failed, say so.

Same for the read-back: verify with `memory_search` when the fact matters, and quote the returned ID.

### What earns a memory

Save a fact when it was expensive to learn and will be true next month:

- Host/DB topology, connection recipes, which environment holds which data
- Credential *locations* (never the secrets)
- Traps: a stale doc, a wrong default, a name that collides across environments
- Retention windows, scale figures, and where a number must be measured

Don't save: task state, anything in the repo already, or a number that changes weekly — unless you record when and where it was measured.

### Scope

Default is project scope. Use `scope: "global"` for facts about the user, their tooling, or cross-project infrastructure. A fact about one repo's schema is project-scoped; a fact about an SSH host serving several repos is global.

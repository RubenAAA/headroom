# Rejected: porting the route-table consolidation

- **Status:** rejected (Python-side reorganization)
- **Source:** `docs/notes/upstream-port-backlog.md` group B
- **Summary:** `providers/proxy_routes.py` (+223/-807 net shrink) is route-table
  consolidation in Python. No Rust action unless a route actually misbehaves.

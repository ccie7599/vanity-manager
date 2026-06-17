# Vanity Manager — Scope

**Tier**: 2 (POC with customer handoff potential)
**Owner**: Brian Apley

## Goals

1. Build an Akamai Functions + EdgeWorker redirect engine that replaces Edge Redirector Cloudlet without its per-policy rule-count cap and without requiring property changes for rule mutations.
2. Demonstrate API-driven redirect management with bulk import/export at 10K–100K rule scale.
3. Keep serving latency at edge-speed (sub-5ms p95) at 4200 Akamai locations via EdgeWorker + EdgeKV.
4. Provide an admin UI for non-technical redirect management (marketing, brand, ops teams).
5. Show cost efficiency vs. origin-based solutions: zero IaaS beyond Object Storage.

## Non-Goals

- **DNS/TLS management** — operators handle DNS pointing to Akamai; Akamai handles TLS.
- **TLD / apex domain mapping** — Akamai Functions apps are behind Akamai CDN, so apex-to-origin mapping is outside this stack.
- **Multi-tenancy** — single deployment per operator. Multi-tenant split is a future consideration.
- **Wildcard host matching** — defer until a concrete requirement.
- **A/B testing or traffic splitting** — separate product concern.

## Large-scale engagement — additions (2026-04-29)

Driver: a large redirect-consolidation engagement — tens of thousands of rules across thousands of zones, spread over several legacy systems (CSC, Nginx, WPE, Akamai ER) — at a scale that exceeds per-policy ER limits and a multi-hundred-SAN cert ceiling. Its data shape surfaced three feature gaps to close before demo.

**Now in scope:**
- **Regex path matching** (was non-goal). A meaningful fraction of the legacy WPE sources are regex. Per-host regex manifest, longest-pattern-first ordering, capture-group substitution in target.
- **Property-code template expansion** at import time. Large blocks of `host → short-code` rules expand to literal target URLs against a per-brand template constant. No runtime templating engine.
- **Incremental bulk import** (`POST /import`). Default semantics: **upsert-without-delete** (rules absent from the payload are left in place). Deletes remain explicit. `?mode=replace` provides full-sync semantics (deletes anything not in payload) but does serial inline EdgeKV calls per delete and does not scale beyond ~50 rules in one Functions invocation (30s wall-clock cap). `?async=1` writes Spin KV and queues EdgeKV pushes for the reconcile worker to drain — required for any payload over a few hundred rules. `?force=1` overrides a stuck import lock. Hash-based unchanged detection skips no-op rules (returns `unchanged: N` rather than re-pushing). Response: `{mode, ekv_sync, total, added, updated, unchanged, deleted, errors[], duration_ms}`.
- **Demo dataset**: load tens of thousands of rules (sanitized / synthesized as needed) into a dedicated demo namespace. Source-data cleanup of malformed entries is not in scope — converter logs and skips them.
- **End-to-end demo property** with EW + EKV bound to the demo namespace, DS2 streaming to a dashboard showing match-type distribution, top hosts/targets, and p95 latency by match type.
- **Benchmark suite** updated for the new match-type mix: serving-plane p50/p95/p99 by match type, admin/API cold-load and incremental-upsert timing, EKV propagation tail.
- **Decoupled control-plane reference (post-demo)**: extract EdgeKV signing/push into a `vmctl` CLI so operators can drive serving-plane updates from their own automation (CI/CD, secrets store, k8s, etc.) without running Akamai Functions. The Functions-hosted admin API + UI becomes a reference implementation, not a required component.

**Explicitly out of scope for this engagement:**
- **Cert / SAN strategy** at 10K hostnames (Enhanced TLS multi-cert, CPS-managed automation) — owned by the cert/CPS team.
- **Edge DNS onboarding** at ~10K zones — owned by the DNS team.
- **Hostname onboarding orchestration** — zone → property → edge hostname → cert SAN at scale.
- **Secrets-manager integration** — the operator's internal source-of-truth choice. Their pipeline calls our API; we don't read their secrets store directly.
- **Apex → www auto-redirect** — handled at the DNS layer, not ours.
- **Audit log / per-user RBAC beyond bearer token** — flagged as Tier 2 future work; demo runs on the existing single-token model.

## Exit Criteria — large-scale additions

- [ ] `MatchType::Regex` shipped end-to-end: storage, import validation, EdgeKV manifest, EW evaluation order, admin UI.
- [ ] `.map` → import JSON converter handles all source formats, expands property-code templates, rewrites `http://` → `https://` targets, reports skipped malformed rows.
- [ ] `POST /import?mode=upsert` upserts without deleting; idempotent on re-run; returns `{added, updated, unchanged, errors}`; concurrency-locked per namespace.
- [ ] Tens of thousands of rules loaded into demo namespace; demo property serves redirects end-to-end with `X-Vanity-*` headers visible in DS2.
- [ ] DS2 dashboard live with match-type distribution, top-N panels, and p95-by-match-type latency.
- [ ] Benchmark report: serving p50/p95/p99 across match types at 100/1K/10K rps; cold-load and incremental-upsert timings; EKV propagation tail.

## Exit Criteria

- [ ] All four Spin components build and deploy to Akamai Functions via `make deploy`.
- [ ] EdgeWorker bundle builds and activates on staging + production networks via `make ew-deploy-staging` / `make ew-deploy-prod`.
- [ ] Admin API: full CRUD + bulk import of 1K+ redirects with inline EdgeKV write-through.
- [ ] EdgeWorker: <5ms p50 latency for exact-match redirects against EdgeKV at edge.
- [ ] Functions fallback: triggered on EdgeKV transient errors, not on misses.
- [ ] Reconcile worker drains `pending-ekv:*` queue end-to-end after simulated EdgeKV outage.
- [ ] Cold-start: Spin KV rehydrates from S3 snapshot when empty.
- [ ] Versioned S3 snapshots: `catalog/rules.json` (current) + `catalog/history/rules-{ts}.json` (immutable).
- [ ] Admin UI functional: rule CRUD, host case_sensitive toggle, pending-sync visibility, import/export.
- [ ] DS2 captures EdgeWorker `X-Vanity-*` response headers; per-rule and per-target metrics queryable downstream.
- [ ] Benchmark: side-by-side latency comparison vs Redirect Cloudlet on dual-hostname property.
- [ ] Rule changes never require property activation — verified with a staging hostname that adds a new path via API only.

## Architecture Constraints

- **No SQLite**, **no external databases**, **no LKE/VM infrastructure**. Persistence is Spin KV (globally consistent via CosmosDB under the hood) and Linode Object Storage (source of truth).
- Rust for all Functions components (wasm32-wasip1 target).
- Query-string token auth for admin API.
- EdgeWorker JS matches Akamai's JavaScript sandbox (no browser APIs, no `window`/`document`).
- EdgeKV item size limit: 512KB per item — the data model uses individual exact keys + one prefix manifest per host to stay within this ceiling.
- All rule mutations are write-through to EdgeKV with a pending-marker queue for retry. No manual sync step.
- Property Manager is configured once; rule changes never require property edits.

# Vanity Manager — Architecture Decision Records

## ADR-001: Akamai Functions as control plane, EdgeWorker + EdgeKV as data plane

**Status**: Accepted  **Date**: 2026-04-14

**Context**: Redirect logic must run in 4200 CDN locations (milliseconds matter for inline redirects), but the control plane (admin UI, CRUD API, management) only needs to run in a handful. Akamai Functions runs in ~30 DCs; EdgeWorkers run in 4200.

**Decision**: Split the stack:
- **Control plane** — Akamai Functions (Spin/Rust) hosts the admin UI, REST API, and the sync orchestrator. Writes land in Spin KV (globally consistent), mirror to Linode Object Storage (source of truth), and push to EdgeKV.
- **Data plane** — EdgeWorker (JS) runs at all 4200 locations, reads redirect rules from EdgeKV, serves 301/302 inline. DataStream 2 captures metrics passively.

**Consequences**: Two runtime environments (Rust/Wasm + JS) — deliberate; control plane benefits from Rust's correctness and perf, data plane runs on the only language Akamai supports at that tier. Rule changes never cross Functions → EdgeWorker compile boundary; EdgeKV is the ABI.

## ADR-002: Spin KV (globally consistent) as hot store; Object Storage as source of truth

**Status**: Accepted  **Date**: 2026-04-14

**Context**: Earlier design used Spin SQLite — **not available on Akamai Functions** and not distributed-consistent even if it were. Need a data store that works on the hosted platform and stays consistent across the ~30 Functions DCs.

**Decision**:
- **Spin KV** holds the hot store: all rules, host metadata, counters, pending-push markers. Backed by CosmosDB under the hood, so reads and writes are globally consistent.
- **Linode Object Storage** holds the durable source of truth: a `catalog/rules.json` snapshot written on every mutation, plus immutable `catalog/history/rules-{timestamp}.json` copies for rollback.
- On cold start, the admin-api component seeds Spin KV from the current S3 snapshot if KV is empty (sentinel `meta:seeded` prevents re-seed).

**Consequences**: Mutations are inline sync to Spin KV (strong consistency) and best-effort to Object Storage (failure logged, not fatal). Manual SigV4 header-based auth (an earlier internal project found presigned URLs don't work against Linode). Backup/restore is via the export endpoint or by re-seeding from a history snapshot.

## ADR-003: EdgeWorker + EdgeKV, not CDN caching, as the forward-deployment strategy

**Status**: Accepted  **Date**: 2026-04-14 (supersedes an earlier "cache 301s in CDN" proposal)

**Context**: The first proposal (Functions serves 301s with `Cache-Control`, CDN caches them for speed) was rejected. Redirects are inline with site performance — the CDN cache TTL creates staleness after rule changes and doesn't keep logic and rules co-located.

**Decision**: EdgeWorker is the primary serving path. It reads EdgeKV on every request (not cached at the property level) so rule changes propagate in ~5–10 seconds via EdgeKV alone. Functions is a fallback-only resolver invoked by EW on EdgeKV transient errors.

**Consequences**:
- Propagation time is EdgeKV-bound, not CDN-cache-bound. Rule changes are live in seconds.
- Every EW invocation costs at least one EdgeKV read. We minimize cost with a per-host `_hosts::{host}` sentinel: unmanaged hosts are rejected with a single read.
- The hot path for managed hosts is two reads (sentinel + exact). Prefix-only matches cost three (sentinel + exact miss + manifest).

## ADR-004: EdgeKV data model — individual exact keys + per-host prefix manifest

**Status**: Accepted  **Date**: 2026-04-14

**Context**: EdgeKV is key-value with a 512KB per-item limit. Longest-prefix matching is not native. Two candidate shapes evaluated:

- **Per-host document** — one key per host holding all rules. Simple (always 1 read) but hits the 512KB limit when a single host has thousands of path-level rules (~8K rules ≈ 800KB raw).
- **Individual exact keys + per-host prefix manifest** — one key per exact rule, one manifest per host for prefix rules.

**Decision**: Individual keys + prefix manifest. Both extremes fit:
the **wide** shape (many hosts × few rules each — vanity/portfolio-style)
and the **tall** shape (few hosts × many rules each — mass-marketing-style).
Manifest is pre-sorted longest-first on the control plane so the EdgeWorker
does an O(n) scan with first-match-wins, no runtime sort.

**Consequences**:
- Hot-path exact match: 1 EKV read after host sentinel. 2 reads total.
- Prefix-only match: 2 EKV reads after host sentinel. 3 reads total.
- Single prefix-rule mutation rebuilds the host's full manifest and re-pushes — acceptable because prefix rules per host are typically small (<50).

## ADR-005: Broad EdgeWorker invocation, no path-match in Property Manager

**Status**: Accepted  **Date**: 2026-04-14

**Context**: Redirect Cloudlet's killer feature is that rule changes never require property activation. If the property says "fire EW when path matches /mortgage/*", adding `/refinance` as a new redirect means editing and activating the property — the Cloudlet problem recurs.

**Decision**: The EdgeWorker fires on every non-`/api/*` request. All host/path selectivity happens inside the EW via EdgeKV lookups. The `_hosts::{host}` sentinel is the fast-reject for unmanaged hosts, keeping the pass-through cost minimal (1 EKV read, genuinely-not-found is the EKV fastest path).

**Consequences**:
- Rule changes — including brand-new hostnames and paths — are EdgeKV writes only. Property is never touched after initial install.
- Every property request pays one EKV read, even pass-through. The `_hosts::` sentinel is idempotent and cacheable at the EW regional reader.
- Benchmark must include EW overhead on unmanaged traffic as a cost axis vs Cloudlet (which has zero overhead on skips).

## ADR-006: Write-through sync with pending-marker queue, drained by cron

**Status**: Accepted  **Date**: 2026-04-14

**Context**: Need durable eventual consistency to EdgeKV even when the EdgeKV management API is transiently unavailable. Pattern ported from an earlier internal project's `pending:{ingest_id}` reconcile loop.

**Decision**: Every mutation tries an inline EdgeKV push. On failure, a `pending-ekv:{target}` marker is written to Spin KV. A reconcile worker drains these markers on schedule via Spin cron trigger (or external HTTP cron if Akamai Functions doesn't honor cron).

**Consequences**:
- Single-writer Spin KV makes this safe: no distributed locks needed.
- Markers track `attempts` and `last_error_at` for debugging. Exposed via `/v1/pending` for the admin UI.
- Bulk imports never overload the EdgeKV API — imports write Spin KV + S3 immediately, EdgeKV pushes are queued behind markers and drained over time.

## ADR-007: Per-host case sensitivity, not per-rule

**Status**: Accepted  **Date**: 2026-04-14

**Context**: RFC says paths are case-sensitive, but customers often want case-insensitive behavior for user-facing redirects. Cloudlet has per-rule case sensitivity.

**Decision**: Case sensitivity is a per-host flag stored in the `_hosts::{host}` sentinel. When false, the EdgeWorker lowercases the request path before lookup. Admin API stores rule paths as-written; the EW normalizes at read time.

**Consequences**:
- One flag, not N. Simpler admin UI, simpler EdgeKV model.
- Host-level only — can't mix case-sensitive and case-insensitive rules on the same host. In practice this is how customers set policies, so acceptable.
- Cloudlet parity gap documented. If a customer demands per-rule, we can add a parallel lowercased exact key on writes and do a second read on case-sensitive miss — deferred until needed.

## ADR-008: Metrics via DS2 capturing EW response headers, not a beacon subrequest

**Status**: Accepted  **Date**: 2026-04-14 (revised — earlier version proposed a `/_beacon/*` subrequest)

**Context**: Akamai Functions has limited observability primitives. DS2 is the canonical Akamai telemetry path and exports cleanly into ClickHouse, Hydrolix, Splunk, etc. The Akamai property's DS2 stream already produces one log line per request the property serves — including EdgeWorker-served redirects. The only data missing from the standard fields is the rule provenance (which rule matched, what target URL was sent).

**Decision**: The EdgeWorker sets four custom response headers on every served redirect:

- `X-Vanity-Manager`: `magnum` (EW path) or `vanity` (Functions fallback)
- `X-Vanity-Match-Type`: `exact` / `prefix`
- `X-Vanity-Src-Path`: the stored source path of the rule that fired
- `X-Vanity-Target`: outgoing Location URL

DS2 is configured to capture these as **Response Header** dataset fields. One DS2 line per redirect, fully self-describing, queryable in ClickHouse without any join.

**Consequences**:
- No beacon subrequest, no `/_beacon/*` admin route, no fire-and-forget HTTP call from the EW. Half the log volume vs. the original beacon design, no correlation step downstream.
- 30-second DS2 lag is acceptable for aggregates, alerting, and zombie-path analysis.
- Real-time needs (admin UI sync status, rule-change validation) go through the admin API directly against Spin KV — no dependency on DS2.
- DS2 dataset fields must be configured (one-time) in the Akamai console to capture custom response headers — see `docs/ds2-fields.md`.

## ADR-009: Import-time `case_insensitive` signal lifts to host meta + path lowercase fix

**Status**: Accepted  **Date**: 2026-04-29

**Context**: ADR-007 made case sensitivity per-host. Large-customer bucketing surfaced two findings: (1) ~60% of that customer's WPE patterns use the PCRE `(?i)` inline flag (which JS RegExp does not support), and (2) the existing per-host `case_sensitive=false` had a latent bug — rule paths were stored case-preserved but the EdgeWorker lowercases the lookup path, so any non-lowercase stored path silently misses. The fix and that use case point at the same change.

**Decision**:
1. **Path lowercase at store time** when the host is case-insensitive. Implemented via a helper (`apply_ci`) that runs at every rule-mutation entry point in the admin API (create, update, import). After the helper runs, the rule's path is lowercased iff the host is ci.
2. **`case_insensitive` field on Redirect** as an import-time signal only. When true on input, `apply_ci` flips the host's `case_sensitive` to false and the rule's path is lowercased before storage. The flag is then cleared on the rule (case-insensitivity is host-scoped in storage, per ADR-007) and is omitted from output via `skip_serializing_if`.
3. **PUT does not flip ci.** Updating a rule with `case_insensitive=true` is a no-op — flipping a host's case-sensitivity while it has existing mixed-case keys would orphan them. To migrate a host to ci, re-import its rules.

**Consequences**:
- Per-host, not per-rule, remains the granularity (consistent with ADR-007).
- `(?i)` patterns in WPE downconvert cleanly: the converter (#28) strips `(?i)` and emits `case_insensitive=true` on the resulting exact/prefix rule. The first such rule for a host flips host meta on first import.
- The customer's mixed hosts (host has both ci and cs rules) collapse to ci-uniform after import. Verified safe against their data: the cs rules on mixed hosts are property-code slugs where lowercasing causes no collisions.
- Bug fix: `case_sensitive=false` hosts now actually resolve correctly. Previously a ci=false host with a path `/Foo` would miss every lookup because EW lowercases to `/foo`.

## ADR-010: Functions → EKV is the production write path; direct EKV only for one-off bulk loads

**Status**: Accepted  **Date**: 2026-04-29

**Context**: Loading ~24K rules (~37K EdgeKV items) into a fresh demo namespace surfaced two hard limits:

- **Akamai Functions has a 30s wall-clock per invocation.** Inline EKV pushes from the admin API serialize over outbound HTTP — even after wrapping the drain loop in `futures::future::join_all`, drain budget 25 still hits the 30s cap (we measured ~2–4s per EKV PUT regardless of concurrency level inside the Spin Wasm component).
- **Spin KV writes also dominate at scale.** 200 net-new rules per `/import` chunk takes ~25s of Spin KV ops (`apply_ci`, `get_json` existing, `set_json`, `ensure_host_meta`, enqueue marker — ~5 ops × ~25ms each).

The drain queue + reconcile worker drain markers in serial because the Spin SDK's `outbound HTTP` does not actually parallelize within one component invocation in this runtime. Bumping drain budget down to fit the cap reduces throughput proportionally — not a fix.

**Decision**:

- **Production updates flow Functions → EdgeKV.** This is what the customer's hourly CI/CD pipeline will exercise. Payloads are small (deltas, not full state), so the 30s cap is not a constraint and the unchanged-detection in `/import` keeps the work bounded.
- **One-off bulk loads bypass Functions.** Use `tools/convert/push_ekv.py` (or the future `vmctl` per task #35), signing EdgeGrid requests directly from the operator's environment to EdgeKV. We measured ~2 EKV PUTs/sec/process from a remote VM with `requests` + `EdgeGridAuth`; multi-process scaling did not help (~8 PUTs/sec aggregate at 4 processes × 25 threads, same as a single 80-thread process). The bottleneck appears to be EdgeKV API per-account latency, not client concurrency.
- **For demo and customer migration, treat the initial load as a separate one-time operation.** Document this in the customer brief — "we can load N rules in ~M minutes via direct EKV; from then on incremental updates flow through your CI/CD pipeline → Functions in seconds."

**Consequences**:

- The admin API stays the system of record for incremental updates and the user-facing surface (UI, audit, sync state). It does not need to handle bulk-cold-load throughput.
- The `vmctl` CLI (task #35) becomes a first-class deliverable, not a nice-to-have. It's the canonical "operator runs this once at migration time" tool and the same code path customers would use if they wanted to bypass our Functions hosting entirely (per SCOPE — the customer's stack-affinity preference).
- The reconcile worker remains useful for retrying transient EKV failures on the production write path. Its budget stays small (we keep DRAIN_BUDGET = 25) since it operates within the Functions 30s window. We do not try to use it as a bulk-load mechanism.
- Snapshot publish and the Spin-KV-as-system-of-record architecture (ADR-002) is not changed — the demo's data-plane-only load is an accepted divergence for one-off operations and is documented in tools READMEs.

## ADR-011: Pluggable KV backend — Spin KV (default) + NATS adapter (high-rule-count demos)

**Status**: Accepted  **Date**: 2026-04-30

**Context**: ADR-002 picked Spin KV (CosmosDB-backed) as the hot store. That choice is correct for the prod control plane and remains the documented architecture. But Spin KV does not expose a prefix-scan primitive; `Store::get_keys()` returns every key in the namespace, and at ~10K+ keys the call alone takes longer than Akamai Functions' 30s wall-clock — observed concretely on a large demo at ~24K rules, where `/stats` and `/rules` both timed out. Per-host indexes and counters (Tier 1 from the scaling discussion) would mitigate this within Spin KV but don't generalize: any "list all rules" or "scan by prefix" path remains stuck behind the same enumeration limit.

**Decision**: Introduce a thin pluggable backend behind a `Backend` enum in `shared::store`. Two implementations ship:

- **`Backend::Spin`** — wraps `spin_sdk::key_value::Store`. Default. The "we run on managed Spin KV" story for prod and small customers. Behavior unchanged.
- **`Backend::Nats`** — HTTP client to the project-nats-kv adapter (`/v1/kv/<bucket>/...`). The adapter does prefix scanning in Go outside the Wasm sandbox, so listing 24K+ keys stays sub-second.

Selection is per-deployment via Spin variables:

- `kv_backend` — empty / `"spin"` (default) or `"http://<adapter-url>"`
- `kv_nats_bucket`, `kv_nats_token` — required when the URL form is used

Higher-level helpers (`shared::kv::get_json`, `set_json`, counters), the sync orchestrator, and the admin handlers all consume `&Backend` rather than a Spin-specific store. That makes both paths build the same code; only the runtime configuration differs.

The data plane (EdgeWorker reads of EdgeKV) is unaffected. EdgeKV remains the serving substrate at the 4,200 PoPs.

**Why not just add Tier-1 indexes inside Spin KV?**

We will add them anyway (cached counters, per-host rule-key indexes) — those are a separate piece of work that improves Spin KV performance for prod-shaped data. But:

- They don't make `/rules` listing scale past the 30s cap once an individual host has tens of thousands of rules; a real prefix scan does.
- They don't help any future "search rules by target" or "audit who edited what" path that would need richer scans.
- The abstraction lets us add other backends later (DynamoDB, Postgres, customer-provided) without touching handlers.

**Why not move *every* deployment to NATS?**

Because the Spin KV story is part of the value proposition — "this runs on the platform you already have." The default deployment must continue to demonstrate that. NATS is for situations where the rule volume exceeds what Spin KV can list, OR where the customer has their own preferred backing store and we're swapping a backend in without rewriting the control plane.

**Consequences**:

- Two backends to maintain. The HTTP path adds latency (a network hop per KV op vs. in-Wasm Spin KV); accept it for the demo workloads where it lives.
- Key encoding: NATS subjects allow only `[A-Za-z0-9_-.]`, so our `:`/`|`-separated keys are translated at the network boundary into `__HHHH`-escaped subjects. Spin KV continues to use the original keys. The translation is purely internal to `Backend::Nats`; callers never see encoded keys.
- Counter atomicity differs: the Spin path is read-modify-write (single-writer admin, fine in practice); the NATS path uses the adapter's atomic `POST :key/incr`.
- Each deployment must size its kv_backend choice intentionally:
  - **Prod / small customer (≤ a few thousand rules)**: `kv_backend=spin` — keep the Spin KV story, no extra infrastructure.
  - **Demo / large-customer engagement (24K+ rules)**: `kv_backend=http://<nats-adapter>` with a dedicated bucket. Sub-second listing and counts. Customer brief should call out that we *can* run on Spin KV for their scale, this is just the demo's choice.

This preserves both narratives honestly: "this works on Spin KV" (prod) AND "we can scale past Spin's enumeration limit when we need to" (demo).

## ADR-012: S3 catalog snapshot is operator-triggered, not per-mutation

**Status**: Accepted  **Date**: 2026-04-30

**Context**: The original design had every successful mutation (`POST /rules`, `PUT /rules/...`, `DELETE /rules/...`, `POST /import`, `PUT /hosts/...`) call `publish_snapshot()` inline. The snapshot dumps every rule + host meta to `<prefix>/rules.json` in object storage and writes an immutable timestamped history copy. With Spin KV's single-pass `get_keys()` that was tolerable up to a few thousand rules. With the NATS backend (ADR-011) it became fatal — `keys_with_prefix()` is fast, but `publish_snapshot()` then issues N sequential HTTP `GET`s against the adapter to deserialize each value. At 7,900 rules a single import probe took >30s because the post-import snapshot blew the Functions wall-clock; the rule itself wrote in <100ms. The user-visible symptom was "import says `unchanged=1` but the bucket grew" — chunks past the boundary timed out mid-snapshot while their writes had already landed, and the uploader's error path counted the timeout as unchanged.

**Decision**: Decouple snapshot publish from the mutation path. None of `create_rule`/`update_rule`/`delete_rule`/`import_rules`/`update_host` calls `publish_snapshot()` anymore. A new explicit endpoint `POST /api/v1/snapshot` triggers a publish, intended to be called by an operator or scheduled job after a bulk load. The reconcile worker can also be wired to publish on its cron (next iteration).

**Why this is safe**:

- KV (Spin or NATS) is the source of truth at runtime — handlers never read from the S3 snapshot during request handling.
- `ensure_seeded()` only consults the snapshot on cold-start when the KV namespace is empty AND `meta:seeded` is unset. Both production paths (Spin KV is cluster-replicated; NATS bucket runs R3 replicas) preserve their data across restarts, so cold-empty is rare and the worst case is a short window where a freshly-seeded instance lacks rules added after the last operator-triggered snapshot.
- The previous "snapshot every mutation" guarantee was already best-effort (`println!` on failure, no retries) — operators were never able to rely on it as a hard durability boundary. Making it explicit keeps the same semantics with honest framing.

**Consequences**:

- Cold-start seeding loses freshness as a function of how often operators run `/snapshot`. For NATS-backed deployments where the bucket is durable, this is a non-issue. For Spin KV deployments where the namespace can theoretically empty, document that `/snapshot` should run after large imports or on a schedule.
- The `/snapshot` endpoint itself can take longer than 30s on large catalogs (it's still N sequential GETs). Operators run it from a long-lived client (CLI, cron) rather than the browser. Future work can move snapshot publish to the reconcile worker so it runs in a fresh wall-clock budget per chunk.
- Removed the `unchanged-during-timeout` counting bug — `/import` now returns within seconds even at 24K rules.

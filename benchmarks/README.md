# Vanity Manager Benchmarks

Side-by-side comparison vs Redirect Cloudlet on the same Akamai property,
measuring latency and scale limits.

## Setup — dual-hostname property

On a single Akamai property:

- `redirects-vm.example.com` — routed through the Vanity Manager EdgeWorker
- `redirects-cl.example.com` — routed through a Redirect Cloudlet policy

Load the same rule set into both:

1. Generate rules: `python3 generate-rules.py --count 10000 --hosts 10 > rules-10k.json`
2. Import to Vanity Manager: `curl -X POST "$VM_URL/api/v1/import?token=..." -d @rules-10k.json`
3. Export to Cloudlet CSV + upload via Cloudlet Policy Manager (manual today; scripting TBD)
4. Wait for EKV propagation (~10s) and Cloudlet activation (~10–15 min)

## Running

### Latency + throughput

```bash
# against Vanity Manager
k6 run -e TARGET_HOST=redirects-vm.example.com \
       -e RULES_JSON=rules-10k.json \
       -e RPS=500 -e DURATION=2m \
       k6-latency.js

# against Cloudlet
k6 run -e TARGET_HOST=redirects-cl.example.com \
       -e RULES_JSON=rules-10k.json \
       -e RPS=500 -e DURATION=2m \
       k6-latency.js
```

### Scale tests (rule-count ceiling)

```bash
for N in 1000 10000 50000 100000; do
  python3 generate-rules.py --count $N --hosts 10 > rules-$N.json
  # import, wait for EKV propagation, run latency test, record results
done
```

Track EdgeKV propagation time per batch: after import, poll `/v1/stats` until
`pending_ekv_pushes == 0`. Record this as "time-to-full-sync."

### Shape tests (customer distributions)

- **Tall shape** (few hosts × many rules each): `--count 8000 --hosts 5 --shape tall`
- **Wide shape** (many hosts × one rule each): `--count 12000 --hosts 12000 --shape wide`

## Metrics captured

From k6:
- `redirect_latency` — latency of served redirect responses (the hot path)
- `miss_latency` — latency when there's no matching rule (pass-through cost)
- `http_req_duration` — all requests
- `redirects_served` — total count
- `miss_rate` — fraction of requests that didn't match a rule

From `/api/v1/stats`:
- `pending_ekv_pushes` — sync queue depth
- `ekv_pushes_total` / `ekv_errors_total` — push success/failure rates
- `beacon_hits_total` — EW-served redirect counts

From DS2:
- Per-host, per-path distribution
- Geo distribution
- Full request log for offline analysis in ClickHouse / Hydrolix

## What we expect to see

| Metric | Vanity Manager | Cloudlet |
|---|---|---|
| Redirect latency p50 | 1–5ms | <1ms |
| Redirect latency p95 | 5–20ms | 1–3ms |
| Redirect latency p99 | 10–50ms | 5–10ms |
| Unmanaged-path overhead | ~1ms (one EKV read) | ~0ms (skip) |
| Rule count ceiling | 100K+ | ~5K per policy |
| Rule propagation time | 5–10s | 10–15 min |
| Bulk import throughput | Thousands/sec (Spin KV) | Minutes per policy reload |

## Open follow-ups

- CSV importer for Cloudlet-exported policies (migration path).
- Per-region latency breakdown via DS2 geo fields.
- Long-tail tests — verify EdgeWorker cold-start behavior at regional readers
  after low-traffic periods.

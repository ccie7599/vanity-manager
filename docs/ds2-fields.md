# DS2 Fields — Capturing Every Request

The Akamai property's DataStream 2 stream produces one log line per
request the property serves. That covers **all three** request classes
in this system, not just the EdgeWorker redirects:

1. **EW-served redirects** — Magnum mode happy path, `respHeader.X-Vanity-Manager = magnum`
2. **Functions-fallback redirects** — EdgeKV transient failure, `respHeader.X-Vanity-Manager = vanity`
3. **Admin / API / reconcile traffic** — `reqPath` starts with `/api/`, `/_reconcile/`, etc., no `X-Vanity-*` headers

DS2 captures all three uniformly. No beacon subrequest is needed — Akamai
already records every request — we just surface the rule provenance via
response headers on the redirect cases.

## Response headers the EdgeWorker emits

Set on every redirect response (both Magnum / EW path and Vanity /
Functions fallback path):

| Header | Value | Meaning |
|---|---|---|
| `X-Vanity-Manager` | `magnum` or `vanity` | Which serving plane handled the request |
| `X-Vanity-Match-Type` | `exact` or `prefix` | How the rule matched |
| `X-Vanity-Src-Path` | e.g. `/mortgage` | Stored source path of the rule that fired |
| `X-Vanity-Target` | e.g. `https://new.example.com/lending` | Outgoing redirect target |
| `X-Redirect-Host` | e.g. `shop-chicago.example.com` | Normalised request host |

## DS2 stream configuration

Standard dataset fields already capture status (1016), reqHost (1005),
reqPath (1013), reqId (1017), client IP (2014), country (2012), bytes
(2010), turnaround (3000) and user agent (1102). Add **Custom Response
Headers** in the DS2 console:

1. Open the `vanity-manager-ds2` stream
2. **Edit Stream → Dataset Fields → Add Response Headers**
3. Add each of the five `X-Vanity-*` / `X-Redirect-*` header names above
4. Re-activate the stream

Each captured header lands in the JSON output as
`responseHeaders.X-Vanity-Match-Type` (etc.), or as flat `respHeader_<name>`
fields depending on the destination format.

## Decoding redirect events downstream (ClickHouse / Hydrolix)

Per **redirect** log line:

- `reqHost` — managed hostname the request came in on
- `reqPath` — what the user asked for
- `status` — 301 / 302 / 307 / 308
- `respHeader.X-Vanity-Manager` — `magnum` (EW served) vs `vanity` (Functions fallback)
- `respHeader.X-Vanity-Target` — full outgoing Location
- `respHeader.X-Vanity-Match-Type` — exact / prefix
- `respHeader.X-Vanity-Src-Path` — which stored rule fired (host + this is the composite primary key in the admin store)

Per **admin / API / reconcile** log line, you get the standard CDN-log
fields with no `X-Vanity-*` headers. Filter or split on `reqPath`:

- `/api/v1/rules`, `/api/v1/rules/...` — CRUD on rules
- `/api/v1/import`, `/api/v1/export` — bulk
- `/api/v1/hosts`, `/api/v1/hosts/...` — host CRUD
- `/api/v1/stats`, `/api/v1/pending`, `/api/v1/test/...` — read-only
- `/api/v1/ui`, `/api/v1/docs`, `/api/v1/readme`, `/api/v1/openapi.yaml`, `/api/v1/architecture.svg`, `/api/v1/features.svg` — UI + docs
- `/_reconcile/drain` — pending-marker drain
- `/api/health` — health check

Useful queries:

**Redirect-side:**
- **Volume per rule:** `GROUP BY reqHost, respHeader.X-Vanity-Src-Path`
- **Zombie paths:** rules in `/api/v1/export` that have zero hits over N days
- **Hot top destinations:** `GROUP BY respHeader.X-Vanity-Target`
- **Magnum vs Vanity ratio (EW health):** `count() WHERE respHeader.X-Vanity-Manager='vanity'` should be near zero in steady state; spikes mean EdgeKV reads are failing at the edge
- **Geo distribution:** `GROUP BY country` (built-in field 2012)

**Admin-side:**
- **Mutation volume:** `count() WHERE reqPath LIKE '/api/v1/rules%' AND reqMethod IN ('POST','PUT','DELETE')`
- **Bulk imports:** `count() WHERE reqPath = '/api/v1/import'`
- **Admin UI sessions:** `countDistinct(clientIp) WHERE reqPath LIKE '/api/v1/ui%'`
- **Reconcile cadence:** `count() WHERE reqPath LIKE '/_reconcile/%' GROUP BY toStartOfMinute(reqTimeSec)`
- **Failed admin auth attempts:** `count() WHERE reqPath LIKE '/api/v1/%' AND status = 401`

If you need per-operator attribution for admin actions, add a custom
request header like `X-Vanity-Operator` to admin API calls and capture
that header as a request-header dataset field in the stream config.

## What is NOT captured

- Pass-through requests where no rule matched (the EW returns without
  `respondWith`, so DS2 still records the request, but with origin's
  status and no `X-Vanity-*` headers — fine, distinguishable by
  presence of the header).
- Real-time / inline redirect counts. DS2 batches every 30s. For
  near-real-time, query `/api/v1/stats` against admin Functions which
  reads counters out of Spin KV directly.

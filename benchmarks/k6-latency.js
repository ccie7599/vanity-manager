// k6 latency + throughput driver. Point at an Akamai property that fronts
// either Vanity Manager or Redirect Cloudlet (use the same rule set in both).
//
// Two modes via env vars:
//   TARGET_HOST  — hostname to pass as Host header (e.g. redirects.example.com)
//   RULES_JSON   — path to rule set JSON (reused from generate-rules.py)
//
// Sampled requests use real rules from the set, so redirect responses are
// authentic 301/302s, not 404 misses. Small fraction of requests hit
// unmanaged paths to measure pass-through overhead.
//
// Usage:
//   k6 run -e TARGET_HOST=redirects-vm.example.com \
//          -e RULES_JSON=benchmarks/rules-10k.json \
//          benchmarks/k6-latency.js

import http from 'k6/http';
import { check, fail } from 'k6';
import { SharedArray } from 'k6/data';
import { Trend, Rate, Counter } from 'k6/metrics';

const TARGET_HOST = __ENV.TARGET_HOST;
if (!TARGET_HOST) fail('TARGET_HOST env var required');
const RULES_JSON = __ENV.RULES_JSON || 'benchmarks/rules.json';
const MISS_RATIO = parseFloat(__ENV.MISS_RATIO || '0.05');

const rules = new SharedArray('rules', () => JSON.parse(open(RULES_JSON)));

export const options = {
  scenarios: {
    steady: {
      executor: 'constant-arrival-rate',
      rate: parseInt(__ENV.RPS || '200'),
      timeUnit: '1s',
      duration: __ENV.DURATION || '60s',
      preAllocatedVUs: parseInt(__ENV.VUS || '50'),
      maxVUs: parseInt(__ENV.MAX_VUS || '500'),
    },
  },
  thresholds: {
    http_req_duration: ['p(50)<10', 'p(95)<50', 'p(99)<100'],
    'redirects_served': ['count>0'],
    'misses': ['rate<0.2'],
  },
};

const redirectLatency = new Trend('redirect_latency', true);
const missLatency = new Trend('miss_latency', true);
const redirectsServed = new Counter('redirects_served');
const misses = new Rate('misses');

export default function () {
  let host, path;
  if (Math.random() < MISS_RATIO) {
    // Request a path that probably doesn't have a rule
    host = rules[Math.floor(Math.random() * rules.length)].host;
    path = '/nonexistent-' + Math.random().toString(36).slice(2, 10);
  } else {
    const r = rules[Math.floor(Math.random() * rules.length)];
    host = r.host;
    path = r.path === '/' ? '/' : r.path + (r.match_type === 'prefix' ? '/sub' : '');
  }

  const res = http.get(`https://${TARGET_HOST}${path}`, {
    headers: { Host: host },
    redirects: 0, // don't follow — we're measuring the redirect response
    tags: { match: res => res.status === 301 || res.status === 302 ? 'hit' : 'miss' },
  });

  const isRedirect = res.status === 301 || res.status === 302;
  check(res, {
    'expected status': (r) =>
      r.status === 301 || r.status === 302 || r.status === 404 || r.status === 200,
  });

  if (isRedirect) {
    redirectLatency.add(res.timings.duration);
    redirectsServed.add(1);
  } else {
    missLatency.add(res.timings.duration);
    misses.add(1);
  }
}

export function handleSummary(data) {
  return {
    stdout: textSummary(data),
  };
}

function textSummary(data) {
  const m = data.metrics;
  const p = (name, field) =>
    m[name] && m[name].values[field] !== undefined ? m[name].values[field].toFixed(2) : 'n/a';
  return [
    '',
    '=== Vanity Manager / Cloudlet latency benchmark ===',
    `target: ${TARGET_HOST}`,
    `rules:  ${rules.length} (${RULES_JSON})`,
    '',
    `  redirect_latency   p50=${p('redirect_latency', 'p(50)')}ms  p95=${p('redirect_latency', 'p(95)')}ms  p99=${p('redirect_latency', 'p(99)')}ms`,
    `  miss_latency       p50=${p('miss_latency', 'p(50)')}ms  p95=${p('miss_latency', 'p(95)')}ms`,
    `  http_req_duration  p50=${p('http_req_duration', 'p(50)')}ms  p95=${p('http_req_duration', 'p(95)')}ms  p99=${p('http_req_duration', 'p(99)')}ms`,
    '',
    `  redirects_served:  ${m.redirects_served ? m.redirects_served.values.count : 0}`,
    `  miss_rate:         ${p('misses', 'rate')}`,
    `  error_rate:        ${p('http_req_failed', 'rate')}`,
    '',
  ].join('\n');
}

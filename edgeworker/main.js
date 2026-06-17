//! Vanity Manager EdgeWorker — serves redirects at 4200 Akamai edge locations.
//!
//! Lookup flow (onClientRequest):
//!   1. Read `_hosts::{host}` sentinel — if miss, pass through to origin (unmanaged host)
//!   2. Read `{host}::exact::{path}` — if hit, respondWith(redirect)
//!   3. Read `{host}::prefixes` manifest — if any entry matches, respondWith(redirect)
//!   4. No match → pass through to origin
//!
//! Error handling:
//!   - Genuine miss (404 from EKV) → pass-through (fast)
//!   - Transient error / timeout → subrequest to Functions `/api/resolve` as fallback
//!
//! Metrics: fires a fire-and-forget subrequest to `/_beacon/{host}/{status}/{path}/{mt}`
//! after every served redirect. DS2 captures this at the property layer.
//!
//! Per-host `case_sensitive` flag is carried in the host sentinel; when
//! false, we lowercase the request path before lookup.

import { EdgeKV } from './edgekv.js';
import { edgekv_access_tokens } from './edgekv_tokens.js';
import { httpRequest } from 'http-request';
import { logger } from 'log';

const DEFAULT_NAMESPACE = 'vanity-manager';
const GROUP = 'redirects';
const EKV_TIMEOUT_MS = 200;

// Per-instance host-sentinel cache. EdgeWorker module state survives across
// requests on the same edge server (lifetime is Akamai-controlled, on the
// order of seconds–minutes). Host metadata changes rarely; reading it from
// EdgeKV every request is the dominant tail-cost on the success path. We
// memo aggressively here and accept brief staleness on flip-the-host-flag
// type changes (which are operator actions, not request-path actions).
const HOST_SENTINEL_CACHE = new Map(); // host -> {meta, exp_ms}
const HOST_SENTINEL_TTL_MS = 5 * 60 * 1000; // 5 minutes

// Per-host namespace override. One EW serves multiple deployments by mapping
// the request hostname to its dedicated EdgeKV namespace; falls back to
// DEFAULT_NAMESPACE for hosts not in the table.
const NAMESPACE_BY_HOST = {
  // 'redirects.example.com': 'vanity-manager-alt',
};

function namespaceFor(host) {
  return NAMESPACE_BY_HOST[host] || DEFAULT_NAMESPACE;
}

// EdgeKV accepts only [A-Za-z0-9_-] in item names. This encoder MUST match
// `shared::ekv::encode_ekv_key` in the Rust control plane, byte-for-byte.
function encEkvKey(s) {
  let out = '';
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    const isSafe =
      (c >= 0x30 && c <= 0x39) || // 0-9
      (c >= 0x41 && c <= 0x5a) || // A-Z
      (c >= 0x61 && c <= 0x7a) || // a-z
      c === 0x2d;                 // -
    if (isSafe) {
      out += s[i];
    } else if (c < 0x80) {
      out += '_' + c.toString(16).toUpperCase().padStart(2, '0');
    } else {
      // Non-ASCII: encode each UTF-8 byte.
      const bytes = new TextEncoder().encode(s[i]);
      for (const b of bytes) {
        out += '_' + b.toString(16).toUpperCase().padStart(2, '0');
      }
    }
  }
  return out;
}
const hostSentinelKey = (host) => encEkvKey('_hosts::' + host);
const exactKey = (host, path) => encEkvKey(host + '::exact::' + path);
const prefixesKey = (host) => encEkvKey(host + '::prefixes');
const prefixesShardKey = (host, shard) =>
  shard === 0 ? prefixesKey(host) : encEkvKey(host + '::prefixes-' + shard);

export async function onClientRequest(request) {
  const host = normalizeHost(request.host);
  const path = normalizePath(request.path);
  const debugMode = (request.query || '').indexOf('ewdebug=1') !== -1;
  // performance.now() gives sub-millisecond resolution; Date.now() rounds to
  // ms and will report 0 for fast warm-cache reads.
  const ewT0 = (typeof performance !== 'undefined' && performance.now) ? performance.now() : Date.now();

  const NAMESPACE = namespaceFor(host);
  const ekv = new EdgeKV({ namespace: NAMESPACE, group: GROUP });

  // 1) Host sentinel — fast reject for unmanaged hosts. Memoized in
  //    HOST_SENTINEL_CACHE per EW instance; cuts one EKV read per request
  //    on every cache hit. On miss, fetch + populate cache.
  let hostMeta;
  let debugInfo = `host=${host} hostKey=${hostSentinelKey(host)} ns=${NAMESPACE} grp=${GROUP}`;
  let ekvErr = null;
  const cached = HOST_SENTINEL_CACHE.get(host);
  const nowMs = Date.now();
  if (cached && cached.exp_ms > nowMs) {
    hostMeta = cached.meta;
    debugInfo += ` hostMeta=${JSON.stringify(hostMeta)} (cache_hit)`;
  } else {
    try {
      hostMeta = await ekv.getJson({
        item: hostSentinelKey(host),
        default_value: null,
        timeout: EKV_TIMEOUT_MS,
      });
      HOST_SENTINEL_CACHE.set(host, { meta: hostMeta, exp_ms: nowMs + HOST_SENTINEL_TTL_MS });
      debugInfo += ` hostMeta=${JSON.stringify(hostMeta)} (cache_miss)`;
    } catch (err) {
      ekvErr = String(err && err.message ? err.message : err).slice(0, 200);
      debugInfo += ` ERR=${ekvErr}`;
    }
  }

  // On EKV error or unknown host: pass-through to origin. The previous
  // implementation issued a Functions-fallback subrequest here; that added
  // ~250 ms of tail latency for every transient EKV blip and depended on
  // the property's forward origin pointing at the right Spin app. Letting
  // origin handle it keeps the success-path tail clean and is honest about
  // what the EW can recover from on its own.
  if (ekvErr && !debugMode) {
    return; // pass-through; origin (Functions redirect-handler) will resolve
  }
  if ((!hostMeta || hostMeta.enabled === false) && !debugMode) {
    return;
  }

  const lookupPath = (hostMeta && hostMeta.case_sensitive === false) ? path.toLowerCase() : path;
  debugInfo += ` lookupPath=${lookupPath} exactKey=${exactKey(host, lookupPath)}`;

  // 2) Exact match — single EKV read, the hot path.
  let rule;
  try {
    rule = await ekv.getJson({
      item: exactKey(host, lookupPath),
      default_value: null,
      timeout: EKV_TIMEOUT_MS,
    });
    debugInfo += ` exactRule=${JSON.stringify(rule)}`;
  } catch (err) {
    debugInfo += ` exactERR=${String(err && err.message ? err.message : err).slice(0,200)}`;
    if (debugMode) {
      request.respondWith(200, { 'Content-Type': ['text/plain'] }, `EW DEBUG\n${debugInfo}\n`);
      return;
    }
    return; // pass-through on transient EKV error; origin will resolve
  }

  // 3) Prefix manifest — one more EKV read for shard 0, plus N-1 more if
  // shard 0 declares a multi-shard manifest. Single-shard format is a bare
  // array; multi-shard shard 0 is {n, p:[...]}.
  if (!rule) {
    let entries = [];
    let shardCount = 1;
    try {
      const head = await ekv.getJson({
        item: prefixesKey(host),
        default_value: null,
        timeout: EKV_TIMEOUT_MS,
      });
      if (Array.isArray(head)) {
        entries = head;
      } else if (head && typeof head === 'object' && Array.isArray(head.p)) {
        entries = head.p;
        shardCount = Math.max(1, head.n | 0);
      }
    } catch (err) {
      return; // pass-through on transient EKV error; origin will resolve
    }
    for (let s = 1; s < shardCount; s++) {
      try {
        const more = await ekv.getJson({
          item: prefixesShardKey(host, s),
          default_value: null,
          timeout: EKV_TIMEOUT_MS,
        });
        if (Array.isArray(more)) {
          for (let i = 0; i < more.length; i++) entries.push(more[i]);
        }
      } catch (err) {
        // Soft-fail on a single shard miss — better than 5xx.
        debugInfo += ` shard${s}ERR=${String(err && err.message ? err.message : err).slice(0,80)}`;
      }
    }
    if (entries.length) {
      rule = matchLongestPrefix(entries, lookupPath);
    }
    debugInfo += ` prefixManifestLen=${entries.length} shards=${shardCount}`;
  }

  if (debugMode) {
    debugInfo += ` finalRule=${JSON.stringify(rule)}`;
    request.respondWith(200, { 'Content-Type': ['text/plain'] }, `EW DEBUG\n${debugInfo}\n`);
    return;
  }

  if (!rule) {
    // Host is managed but this path has no rule — continue to origin.
    return;
  }

  // Build Location.
  const matchType = rule.p ? 'prefix' : 'exact';
  const location = buildLocation(rule, path, request.query, /*isPrefix=*/ !!rule.p);

  // Rule provenance lands on the response itself; DS2 captures any
  // response headers configured at the property/stream level. No beacon
  // subrequest needed — see docs/ds2-fields.md.
  // Server-Timing: ew;dur=<ms> — EW handler self-time (sentinel read +
  // exact lookup + manifest scan + response build). NOT total edge time
  // and NOT client RTT — those include TLS, network, and Akamai routing
  // outside the EW. Browser-side rigs read this via
  // PerformanceResourceTiming.serverTiming[] (gated by Timing-Allow-Origin,
  // which the property emits). Use performance.now() so we don't round
  // sub-millisecond warm reads to 0.
  const tEnd = (typeof performance !== 'undefined' && performance.now) ? performance.now() : Date.now();
  const ewMs = (tEnd - ewT0).toFixed(2);
  request.respondWith(
    rule.s,
    {
      Location: [location],
      'Cache-Control': ['public, max-age=86400'],
      'X-Vanity-Manager': ['magnum'],
      'X-Vanity-Match-Type': [matchType],
      'X-Vanity-Src-Path': [matchType === 'prefix' ? rule.p : lookupPath],
      'X-Vanity-Target': [rule.t],
      'X-Redirect-Host': [host],
      'Server-Timing': [`ew;dur=${ewMs};desc="EW handler self-time (ms)"`],
    },
    ''
  );
}

// ---------------------------------------------------------------------------

function normalizeHost(h) {
  return (h || '').split(':')[0].toLowerCase();
}

function normalizePath(p) {
  let s = p || '/';
  if (!s.startsWith('/')) s = '/' + s;
  while (s.indexOf('//') !== -1) s = s.replace(/\/\//g, '/');
  if (s.length > 1 && s.endsWith('/')) s = s.slice(0, -1);
  return s;
}

function matchLongestPrefix(prefixes, path) {
  // Manifest is pre-sorted longest-first on the control plane, so the first
  // matching entry wins.
  for (let i = 0; i < prefixes.length; i++) {
    const r = prefixes[i];
    if (path === r.p || path.indexOf(r.p + '/') === 0) return r;
  }
  return null;
}

function buildLocation(rule, requestPath, requestQuery, isPrefix) {
  let loc = rule.t;

  // Prefix + preserve_path → append unmatched suffix (rule.p is the prefix).
  if (isPrefix && rule.pp === 1 && requestPath.length > rule.p.length) {
    const suffix = requestPath.slice(rule.p.length);
    loc = loc.replace(/\/$/, '') + suffix;
  }

  // preserve_query → forward original query string.
  if (rule.pq === 1 && requestQuery) {
    loc = loc + (loc.indexOf('?') !== -1 ? '&' : '?') + requestQuery;
  }
  return loc;
}

async function fallbackToFunctions(request, host, path, err) {
  logger.warn(`EKV error for ${host}${path}: ${err && err.message ? err.message : err}`);
  try {
    const url =
      '/resolve?host=' + encodeURIComponent(host) + '&path=' + encodeURIComponent(path);
    const resp = await httpRequest(url, { method: 'GET', timeout: 300 });
    if (resp.ok) {
      const body = await resp.json();
      if (body.match && body.rule) {
        const r = body.rule;
        const rule = {
          t: r.target_url,
          s: r.status_code,
          pp: r.preserve_path ? 1 : 0,
          pq: r.preserve_query ? 1 : 0,
          p: r.match_type === 'prefix' ? r.path : undefined,
        };
        const lookupPath = path; // fallback uses raw path; Functions handles case flag
        const location = buildLocation(rule, lookupPath, request.query, !!rule.p);
        request.respondWith(
          rule.s,
          {
            Location: [location],
            'Cache-Control': ['public, max-age=300'],
            'X-Vanity-Manager': ['vanity'],
            'X-Vanity-Match-Type': [r.match_type],
            'X-Vanity-Src-Path': [r.path],
            'X-Vanity-Target': [r.target_url],
          },
          ''
        );
        return;
      }
    }
  } catch (fallbackErr) {
    logger.error(
      `Functions fallback failed for ${host}${path}: ${fallbackErr && fallbackErr.message ? fallbackErr.message : fallbackErr}`
    );
  }
  // Both EKV and fallback failed — let origin handle it.
}

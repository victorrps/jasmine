# Deferred Infrastructure Work

Items intentionally pushed past the v1 single-instance topology. None of
these block the current `/v1/parse` and `/v1/extract` paths — they all
become load-bearing only when we move beyond one box or one process.

## 1. Idempotency cache — pluggable persistence

**Today** (`src/services/idempotency.rs`): in-process `Mutex<LruCache>`
keyed by `SHA-256(api_key_id || idempotency_key)`. 24 h TTL, 1 MiB body
cap, 10 000 entries (~10 GiB worst case; ~640 MiB if `max_body_bytes`
is dropped to 64 KiB). Resets on restart. Per-instance — multi-instance
deployments will see partial coverage and customers will occasionally
get a non-replay on a retry that hit a different node.

**That is acceptable for v1** because the deployment is a single Rust
process behind one ingress. The `Idempotency-Key` contract still holds:
within an instance window, the same key always replays the same body.

**When to revisit:** the day we put a load balancer in front of more
than one instance of the API.

**Options, in order of operational cost:**

1. **Redis (recommended next step).** A shared `SET key value EX 86400`
   replaces the LRU. Sub-millisecond hot path, native TTL, no schema.
   The `IdempotencyCache` struct stays — only the storage trait behind
   it changes. Add a `CacheBackend` trait with `get/put` and an
   `InProcessLru` + `RedisBackend` implementation. No API surface
   change. Cost: one new piece of infra to operate.

2. **RDBMS write-through.** SQLite/Postgres table
   `idempotency_cache(key BLOB PRIMARY KEY, status SMALLINT, body BLOB,
   content_type TEXT, stored_at TIMESTAMP)`. A periodic GC job deletes
   `WHERE stored_at < now() - 24h`. Slower than Redis but reuses the
   existing `sqlx` pool. Reasonable if we don't want to add Redis just
   for idempotency.

3. **Document database (Mongo/Couch/Firestore).** Only worth it if we
   *also* end up storing parse results, batch records, or other
   document-shaped data in the same store. Don't add a doc DB just for
   the cache.

**Decision rule:** when the deployment grows to 2+ instances, add Redis
before the second instance ships. Until then, the LRU is fine and
documented.

## 2. Idempotency cache — persistence beyond restart

Even single-instance, the cache is volatile across restarts. A customer
who retries 30 s after a deploy will get a fresh execution and a fresh
bill. This is intentional for v1 (the rollout pace is low and the cache
mainly defends against immediate client retries) but it's worth knowing.

**When this becomes load-bearing:** when we hit deployments more than
once a day during peak hours, or when a customer asks for an SLA on
"replay across our maintenance windows."

**Fix:** the same RDBMS write-through above gives us restart durability
for free.

## 3. Concurrency cap — coordination across instances

`MAX_CONCURRENT_PARSES` (default 8) is per-instance. The Paddle sidecar
has its own internal queue. If we run N instances, total in-flight work
is `N × MAX_CONCURRENT_PARSES`, which the sidecar may or may not be
sized for.

**Fix when needed:** a Redis-backed semaphore (or just sizing the
sidecar with explicit knowledge of N), driven from the same operator
runbook that adds Redis for the cache.

## 4. Metrics — scrape-vs-push, multi-instance aggregation

`/metrics` is a Prometheus scrape endpoint, unauthenticated, served
inline by the same actix process. For one instance behind a private
network, this is fine.

**When to revisit:**
- More than one instance → Prometheus needs service discovery
  (`actix` instances are stateless, so target labels can be host:port).
  No code change required.
- Public deployment → put `/metrics` behind a separate listen address
  or an IP allowlist at the ingress; do not expose it on the same
  public port as `/v1/*`.
- Multi-tenant SaaS → consider per-tenant labels with cardinality caps
  (today the Family<Labels, _> instances are bounded by endpoint and
  status — adding `tenant_id` would explode cardinality).

## 5. Rate limiting — peer_addr only

`actix-governor` keys on `peer_addr()` because forwarded-IP headers
cannot be trusted on a public ingress. The day we put the API behind a
load balancer that does `X-Forwarded-For` correctly (and only that
LB can reach the app), we can switch to a header-based key extractor
and document the trust boundary.

**Until then:** all rate limiting is effectively per-LB, not per-client.
This is a known limitation, called out in `CLAUDE.md`.

## 6. Graceful shutdown

Today, `SIGTERM` drops in-flight requests on the floor. The semaphore
won't help here — `try_acquire()` returns immediately, so a `SIGTERM`
just stops accepting new work, but pending work in the blocking pool is
killed when the runtime tears down.

**Fix when needed:** install a signal handler that:
1. Stops accepting new connections (actix supports this).
2. Drains the blocking pool with a configurable grace period.
3. Returns 503 to anything that doesn't finish in time.

This becomes important when we need zero-dropped-request deploys.

---

## Decision summary

| Item | Trigger to revisit | Estimated effort |
|---|---|---|
| Idempotency cache → Redis | 2nd instance | 1 day |
| Idempotency cache → RDBMS write-through | Daily deploys during peak | 1 day |
| Concurrency coord across instances | 2nd instance + Paddle pressure | 0.5 day |
| Metrics scrape multi-instance | 2nd instance | 0 (config only) |
| Rate limiting via X-Forwarded-For | LB termination contract | 0.5 day |
| Graceful shutdown | Zero-drop deploy SLO | 1 day |

None of these are blockers. They are written down so future-us doesn't
have to rediscover the trade-offs.

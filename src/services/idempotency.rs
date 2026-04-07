//! In-process idempotency cache for `/v1/parse` and `/v1/extract`.
//!
//! Customers send `Idempotency-Key: <string>` (max 128 chars). The
//! handler hashes `(api_key_id, idempotency_key)` and looks up a
//! previously cached response. On hit the cached body is returned
//! verbatim with `X-Idempotent-Replay: true`. Usage is **not** logged
//! and billing is **not** charged a second time. On miss the request
//! runs normally; on success the response is cached for 24 hours.
//!
//! ## Storage
//!
//! Today: an in-process LRU keyed by `String` (the hex digest of the
//! `(api_key_id, key)` SHA-256). The cache resets on restart and is
//! per-instance — multi-instance deployments will see partial coverage.
//! That is acceptable for the v1 single-instance topology and is
//! documented in `docs/DEFERRED_INFRA.md` for the multi-instance
//! follow-up (Redis or RDBMS write-through).
//!
//! ## Caching policy
//!
//! - Cache **2xx success** and **stable 4xx** (`InvalidPdf`,
//!   `EncryptedPdf`) — same input always produces the same answer.
//! - Do **not** cache 5xx, 503, 504, 401, 403, 429 — these are
//!   transient or auth-related and should be retried fresh.
//! - Cap cached body size (default 1 MiB). Larger responses bypass
//!   the cache entirely and the response includes
//!   `X-Idempotent-Cached: bypassed-too-large`.
//!
//! ## Memory bound
//!
//! `cache_size` * `max_body_bytes` is the worst-case resident set.
//! Defaults: 1 024 entries × 1 MiB ≈ 1 GiB worst case (typical real
//! parse responses are 5–50 KiB so steady-state is ~50 MiB). Operators
//! who need a tighter footprint can lower `max_body_bytes` to 64 KiB,
//! which drops the worst case to ~64 MiB at the cost of bypassing the
//! cache for any document larger than that.
//!
//! ## Mutex poisoning
//!
//! The internal `Mutex` uses poison-recovery (`PoisonError::into_inner`)
//! rather than failing closed. The cached `CachedResponse` values are
//! plain owned data (no interior invariants that a panic could leave
//! half-set), so reusing the data after a panicking operation is safe
//! and dramatically better than silently disabling idempotency for the
//! lifetime of the process.

use actix_web::http::StatusCode;
use lru::LruCache;
use sha2::{Digest, Sha256};
use std::num::NonZeroUsize;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Maximum allowed length for an `Idempotency-Key` header value.
pub const MAX_KEY_LEN: usize = 128;

/// Default LRU capacity. 1 024 entries × `DEFAULT_MAX_BODY_BYTES`
/// (1 MiB) gives a 1 GiB worst-case resident set; typical parse
/// responses are 5–50 KiB so the steady-state footprint is closer to
/// 50 MiB.
pub const DEFAULT_CACHE_SIZE: usize = 1024;

/// Default cache TTL. 24 hours mirrors Stripe's idempotency window.
pub const DEFAULT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Default per-entry body size cap. Larger bodies bypass the cache.
pub const DEFAULT_MAX_BODY_BYTES: usize = 1024 * 1024;

/// A cached response, ready to be replayed verbatim. `status` is stored
/// as a typed `StatusCode` so we cannot accidentally promote a malformed
/// 4xx into a `200 OK` on replay (the previous `u16 + unwrap_or(OK)`
/// shape silently lost the original status on bad input).
#[derive(Clone, Debug)]
pub struct CachedResponse {
    pub status: StatusCode,
    pub body: Vec<u8>,
    pub content_type: String,
    pub stored_at: Instant,
}

/// Per-instance idempotency cache. Internally a `Mutex<LruCache>`;
/// the lock scope is held only for the lookup/insert call so contention
/// is bounded by lookup time, not request time.
pub struct IdempotencyCache {
    inner: Mutex<LruCache<String, CachedResponse>>,
    ttl: Duration,
    max_body_bytes: usize,
}

impl IdempotencyCache {
    /// Build a cache with explicit capacity, TTL, and per-entry body cap.
    pub fn new(capacity: usize, ttl: Duration, max_body_bytes: usize) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).expect("capacity > 0");
        Self {
            inner: Mutex::new(LruCache::new(cap)),
            ttl,
            max_body_bytes,
        }
    }

    /// Build a cache with the default tuning.
    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_CACHE_SIZE, DEFAULT_TTL, DEFAULT_MAX_BODY_BYTES)
    }

    /// Lock the inner cache, recovering from a poisoned mutex. The
    /// `LruCache` value has no broken invariants a panic could leave
    /// behind, so reusing the data is strictly better than failing
    /// closed and silently disabling idempotency forever.
    fn lock(&self) -> MutexGuard<'_, LruCache<String, CachedResponse>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Per-entry body size cap. Responses larger than this bypass the
    /// cache entirely (caller can detect this via `should_cache`).
    #[allow(dead_code)]
    pub fn max_body_bytes(&self) -> usize {
        self.max_body_bytes
    }

    /// Should a response of this size be cached?
    pub fn should_cache(&self, body_len: usize) -> bool {
        body_len <= self.max_body_bytes
    }

    /// Hash `(api_key_id, idempotency_key)` into the cache key. SHA-256
    /// hex digest. Scoped per API key so different tenants cannot
    /// collide.
    pub fn make_key(api_key_id: &str, idempotency_key: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(api_key_id.as_bytes());
        hasher.update(b"\x00");
        hasher.update(idempotency_key.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Look up a cached response. Returns `Some` only when the entry is
    /// still within the TTL window — expired entries are evicted on hit
    /// (lazy reaping).
    pub fn get(&self, key: &str) -> Option<CachedResponse> {
        let mut guard = self.lock();
        let entry = guard.get(key)?.clone();
        if entry.stored_at.elapsed() > self.ttl {
            guard.pop(key);
            return None;
        }
        Some(entry)
    }

    /// Store a response under `key`. Silently no-ops if the body is
    /// over the per-entry cap (caller should check `should_cache`
    /// first to avoid the wasted clone).
    pub fn put(&self, key: String, response: CachedResponse) {
        if response.body.len() > self.max_body_bytes {
            return;
        }
        self.lock().put(key, response);
    }

    /// Number of entries currently held. Test/observability only.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// True when the cache holds zero entries.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for IdempotencyCache {
    fn default() -> Self {
        Self::with_defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(body: &[u8]) -> CachedResponse {
        CachedResponse {
            status: StatusCode::OK,
            body: body.to_vec(),
            content_type: "application/json".into(),
            stored_at: Instant::now(),
        }
    }

    #[test]
    fn make_key_is_per_tenant_scoped() {
        let a = IdempotencyCache::make_key("key-tenant-a", "abc");
        let b = IdempotencyCache::make_key("key-tenant-b", "abc");
        assert_ne!(a, b, "same idempotency key under different tenants must collide-resist");
    }

    #[test]
    fn put_and_get_round_trip() {
        let cache = IdempotencyCache::with_defaults();
        let key = IdempotencyCache::make_key("k", "i");
        cache.put(key.clone(), sample(b"hello"));
        let got = cache.get(&key).unwrap();
        assert_eq!(got.body, b"hello");
        assert_eq!(got.status, StatusCode::OK);
    }

    #[test]
    fn miss_returns_none() {
        let cache = IdempotencyCache::with_defaults();
        assert!(cache.get("nonexistent").is_none());
    }

    #[test]
    fn expired_entries_are_evicted_on_get() {
        let cache = IdempotencyCache::new(10, Duration::from_millis(1), 1024);
        let key = "k".to_string();
        cache.put(key.clone(), sample(b"data"));
        std::thread::sleep(Duration::from_millis(5));
        assert!(cache.get(&key).is_none(), "expired entry must be evicted");
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn put_silently_skips_oversized_bodies() {
        let cache = IdempotencyCache::new(10, DEFAULT_TTL, 8);
        let key = "k".to_string();
        cache.put(key.clone(), sample(b"way too long for an 8-byte cap"));
        assert!(
            cache.get(&key).is_none(),
            "oversized body must not be cached"
        );
    }

    #[test]
    fn should_cache_threshold() {
        let cache = IdempotencyCache::new(10, DEFAULT_TTL, 100);
        assert!(cache.should_cache(50));
        assert!(cache.should_cache(100));
        assert!(!cache.should_cache(101));
    }

    #[test]
    fn cache_survives_mutex_poisoning() {
        use std::sync::Arc;
        let cache = Arc::new(IdempotencyCache::with_defaults());
        cache.put("k".into(), sample(b"before-panic"));

        // Poison the inner mutex from a panicking thread.
        let c2 = cache.clone();
        let _ = std::thread::spawn(move || {
            let _g = c2.inner.lock().unwrap();
            panic!("intentional poison");
        })
        .join();

        // Cache must still serve the previously stored entry rather than
        // failing closed for the rest of the process lifetime.
        let got = cache.get("k").expect("poisoned cache must still serve reads");
        assert_eq!(got.body, b"before-panic");
        cache.put("k2".into(), sample(b"after-poison"));
        assert_eq!(cache.get("k2").unwrap().body, b"after-poison");
    }

    #[test]
    fn lru_eviction_under_capacity_pressure() {
        let cache = IdempotencyCache::new(2, DEFAULT_TTL, 1024);
        cache.put("a".into(), sample(b"1"));
        cache.put("b".into(), sample(b"2"));
        cache.put("c".into(), sample(b"3")); // evicts "a"
        assert!(cache.get("a").is_none(), "oldest entry must be evicted");
        assert!(cache.get("b").is_some());
        assert!(cache.get("c").is_some());
    }
}

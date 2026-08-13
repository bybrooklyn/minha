//! Deterministic, secret-safe primitives for Minha's local cache.
//!
//! This module intentionally does not perform filesystem I/O.  A future store can
//! use the manifests, keys, metadata, and prune plans without making cache
//! policy decisions itself.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    fmt,
    time::{Duration, SystemTime},
};

pub const DEFAULT_MAX_BYTES: u64 = 512 * 1024 * 1024;
pub const DEFAULT_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CachePolicy {
    pub max_bytes: u64,
    pub ttl: Duration,
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            ttl: DEFAULT_TTL,
        }
    }
}

impl CachePolicy {
    pub const fn new(max_bytes: u64, ttl: Duration) -> Self {
        Self { max_bytes, ttl }
    }

    pub fn class_for(&self, class: CacheClass) -> CacheClass {
        class
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheClass {
    /// Content may be reused indefinitely because its key includes all inputs.
    Exact,
    /// Content may be reused until the policy TTL elapses.
    Ttl,
    /// Content must never be read from or written to the cache.
    Never,
}

impl CacheClass {
    pub fn is_cacheable(self) -> bool {
        self != Self::Never
    }
    pub fn expires_at(self, stored_at: SystemTime, policy: CachePolicy) -> Option<SystemTime> {
        match self {
            Self::Exact => None,
            Self::Ttl => stored_at.checked_add(policy.ttl),
            Self::Never => Some(stored_at),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ObservedInput {
    pub name: String,
    pub digest: String,
    pub size: u64,
}

impl ObservedInput {
    pub fn new(name: impl Into<String>, bytes: &[u8]) -> Self {
        Self {
            name: name.into(),
            digest: sha256_hex(bytes),
            size: bytes.len() as u64,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ObservedInputManifest {
    inputs: Vec<ObservedInput>,
}

impl ObservedInputManifest {
    pub fn new<I>(inputs: I) -> Self
    where
        I: IntoIterator<Item = ObservedInput>,
    {
        let mut inputs: Vec<_> = inputs.into_iter().collect();
        inputs.sort_by(|a, b| a.name.cmp(&b.name).then(a.digest.cmp(&b.digest)));
        Self { inputs }
    }

    pub fn observe<I, N>(inputs: I) -> Result<Self, SecretInput>
    where
        I: IntoIterator<Item = (N, Vec<u8>)>,
        N: Into<String>,
    {
        let mut observed = Vec::new();
        for (name, bytes) in inputs {
            let name = name.into();
            if let Some(reason) = secret_reason(&name, &bytes) {
                return Err(SecretInput { name, reason });
            }
            observed.push(ObservedInput::new(name, &bytes));
        }
        Ok(Self::new(observed))
    }

    pub fn inputs(&self) -> &[ObservedInput] {
        &self.inputs
    }
    pub fn is_empty(&self) -> bool {
        self.inputs.is_empty()
    }

    /// Canonical bytes: length-prefixed names followed by digest and size.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for input in &self.inputs {
            write_field(&mut out, input.name.as_bytes());
            write_field(&mut out, input.digest.as_bytes());
            out.extend_from_slice(&input.size.to_le_bytes());
        }
        out
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretInput {
    pub name: String,
    pub reason: String,
}

impl fmt::Display for SecretInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "secret-like input {}: {}", self.name, self.reason)
    }
}
impl std::error::Error for SecretInput {}

pub fn cache_key(namespace: &str, request: &[u8], manifest: &ObservedInputManifest) -> String {
    let mut hasher = Sha256::new();
    write_field_hash(&mut hasher, namespace.as_bytes());
    write_field_hash(&mut hasher, request);
    write_field_hash(&mut hasher, &manifest.canonical_bytes());
    hex(&hasher.finalize())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Return a log-safe copy of text containing common credential forms.
pub fn redact_secrets(input: &str) -> String {
    input
        .lines()
        .map(|line| {
            let lower = line.to_ascii_lowercase();
            if let Some(separator) = line.find(['=', ':']) {
                let key = lower[..separator].trim().trim_matches('"');
                let value = line[separator + 1..].trim();
                let strong_key = [
                    "api_key",
                    "api-key",
                    "password",
                    "token",
                    "secret",
                    "authorization",
                ]
                .iter()
                .any(|candidate| key.ends_with(candidate));
                let credential_value = value.trim_matches('"').to_ascii_lowercase().starts_with("sk-");
                if strong_key || (key.ends_with("key") && credential_value) {
                    return format!("{}<REDACTED>", &line[..=separator]);
                }
            }
            if lower.trim_start().starts_with("bearer ")
                && let Some(marker) = lower.find("bearer ")
            {
                let marker = marker + "bearer ".len();
                return format!("{}<REDACTED>", &line[..marker]);
            }
            let token = line
                .split_whitespace()
                .find(|token| token.len() > 3 && token.starts_with("sk-"));
            if let Some(token) = token {
                return line.replacen(token, "sk-<REDACTED>", 1);
            }
            line.to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn contains_secret(name: &str, bytes: &[u8]) -> bool {
    secret_reason(name, bytes).is_some()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CacheEntry {
    pub key: String,
    pub class: CacheClass,
    pub bytes: Vec<u8>,
    pub stored_at: SystemTime,
    pub last_used_at: SystemTime,
    pub hits: u64,
    pub pinned: bool,
}

impl CacheEntry {
    pub fn size(&self) -> u64 {
        self.bytes.len() as u64
    }
    pub fn is_fresh(&self, now: SystemTime, policy: CachePolicy) -> bool {
        self.class.is_cacheable()
            && self
                .class
                .expires_at(self.stored_at, policy)
                .is_none_or(|at| now < at)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheMetrics {
    pub entries: usize,
    pub bytes: u64,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LookupMode {
    AllowStale,
    FreshOnly,
    /// Do not consult the cache (for a caller's explicit fresh request).
    Bypass,
}

#[derive(Debug, Default)]
pub struct HotCache {
    entries: HashMap<String, CacheEntry>,
    order: VecDeque<String>,
    bytes: u64,
    metrics: CacheMetrics,
    max_entries: usize,
    capacity: u64,
}

impl HotCache {
    pub fn new(capacity: u64) -> Self {
        Self::with_limits(usize::MAX, capacity)
    }

    /// Construct a cache with independent entry-count and byte limits.
    ///
    /// `new` remains the byte-capacity-only compatibility constructor; callers
    /// wiring a configured entry limit should use this method.
    pub fn with_limits(max_entries: usize, max_bytes: u64) -> Self {
        Self {
            max_entries,
            capacity: max_bytes,
            ..Self::default()
        }
    }
    pub fn max_entries(&self) -> usize {
        self.max_entries
    }
    pub fn capacity(&self) -> u64 {
        self.capacity
    }
    pub fn metrics(&self) -> CacheMetrics {
        self.metrics
    }
    pub fn get(
        &mut self,
        key: &str,
        now: SystemTime,
        policy: CachePolicy,
        mode: LookupMode,
    ) -> Option<&[u8]> {
        if mode == LookupMode::Bypass {
            return None;
        }
        let Some(entry) = self.entries.get_mut(key) else {
            self.metrics.misses += 1;
            return None;
        };
        if !entry.is_fresh(now, policy) && mode == LookupMode::FreshOnly {
            self.metrics.misses += 1;
            return None;
        }
        self.metrics.hits += 1;
        entry.hits += 1;
        entry.last_used_at = now;
        touch(&mut self.order, key);
        Some(entry.bytes.as_slice())
    }

    pub fn insert(&mut self, mut entry: CacheEntry) -> bool {
        if entry.class == CacheClass::Never || entry.size() > self.capacity || self.max_entries == 0 {
            return false;
        }
        if let Some(old) = self.entries.remove(&entry.key) {
            self.bytes -= old.size();
            remove(&mut self.order, &entry.key);
        }
        entry.hits = 0;
        self.bytes += entry.size();
        self.order.push_back(entry.key.clone());
        self.entries.insert(entry.key.clone(), entry);
        self.evict_to(self.capacity);
        true
    }

    pub fn remove(&mut self, key: &str) -> Option<CacheEntry> {
        let entry = self.entries.remove(key)?;
        self.bytes -= entry.size();
        remove(&mut self.order, key);
        self.sync_metrics();
        Some(entry)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn evict_to(&mut self, target: u64) {
        while self.bytes > target || self.entries.len() > self.max_entries {
            if !self.evict_one_lru() {
                break;
            }
        }
        self.sync_metrics();
    }

    fn evict_one_lru(&mut self) -> bool {
        let Some(position) = self
            .order
            .iter()
            .position(|key| self.entries.get(key).is_some_and(|entry| !entry.pinned))
        else {
            return false;
        };
        let Some(key) = self.order.remove(position) else {
            return false;
        };
        let Some(entry) = self.entries.remove(&key) else {
            return false;
        };
        self.bytes -= entry.size();
        self.metrics.evictions += 1;
        true
    }

    pub fn prune_selection(&self, now: SystemTime, policy: CachePolicy, target: u64) -> Vec<String> {
        let mut candidates: Vec<_> = self.entries.values().filter(|e| !e.pinned).collect();
        candidates.sort_by_key(|e| (!e.is_fresh(now, policy), e.last_used_at, e.key.clone()));
        let mut bytes = self.bytes;
        candidates
            .into_iter()
            .take_while(|e| {
                let take = bytes > target;
                if take {
                    bytes -= e.size();
                }
                take
            })
            .map(|e| e.key.clone())
            .collect()
    }

    fn sync_metrics(&mut self) {
        self.metrics.entries = self.entries.len();
        self.metrics.bytes = self.bytes;
    }
}

fn touch(order: &mut VecDeque<String>, key: &str) {
    remove(order, key);
    order.push_back(key.to_owned());
}
fn remove(order: &mut VecDeque<String>, key: &str) {
    if let Some(pos) = order.iter().position(|item| item == key) {
        order.remove(pos);
    }
}
fn write_field(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(&(value.len() as u64).to_le_bytes());
    out.extend_from_slice(value);
}
fn write_field_hash<H: Digest>(hasher: &mut H, value: &[u8]) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn secret_reason(name: &str, bytes: &[u8]) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    if [".env", ".pem", ".key", "credentials", "secrets", "token"]
        .iter()
        .any(|part| lower.contains(part))
    {
        return Some("secret-like filename".into());
    }
    let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    let credential_value = |value: &str| {
        let value = value.trim_matches('"');
        value.starts_with("sk-")
            || value.starts_with("ghp_")
            || value.starts_with("gho_")
            || value.contains("-----BEGIN")
    };
    if text.lines().any(|line| {
        line.find(['=', ':']).is_some_and(|separator| {
            let key = line[..separator]
                .trim()
                .trim_matches('"')
                .replace(['-', ' '], "_");
            let value = line[separator + 1..].trim();
            let strong_key = ["api_key", "password", "token", "secret", "authorization"]
                .iter()
                .any(|candidate| key.ends_with(candidate));
            (strong_key && !value.is_empty()) || (key.ends_with("key") && credential_value(value))
        })
    }) {
        return Some("contains a credential assignment".into());
    }
    if text.lines().any(|line| line.trim_start().starts_with("bearer ")) {
        return Some("contains a bearer credential".into());
    }
    if text.split_whitespace().any(|token| {
        token.len() > 3 && token.starts_with("sk-") && token[3..].chars().any(|c| c.is_alphanumeric())
    }) {
        return Some("contains a sk- prefixed credential".into());
    }
    [
        "api_key=",
        "api-key:",
        "authorization: bearer ",
        "private key",
        "password=",
    ]
    .iter()
    .find(|marker| text.contains(**marker))
    .map(|marker| format!("contains {marker}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(key: &str, class: CacheClass, size: usize, at: SystemTime) -> CacheEntry {
        CacheEntry {
            key: key.into(),
            class,
            bytes: vec![0; size],
            stored_at: at,
            last_used_at: at,
            hits: 99,
            pinned: false,
        }
    }

    #[test]
    fn defaults_and_classes_are_safe() {
        let policy = CachePolicy::default();
        assert_eq!(policy.max_bytes, 512 * 1024 * 1024);
        assert_eq!(policy.ttl, Duration::from_secs(30 * 24 * 60 * 60));
        assert!(!CacheClass::Never.is_cacheable());
        assert!(entry("key", CacheClass::Exact, 0, SystemTime::now()).is_fresh(SystemTime::now(), policy));
    }

    #[test]
    fn manifests_and_keys_are_order_independent() {
        let a = ObservedInputManifest::observe([("b", b"two".to_vec()), ("a", b"one".to_vec())])
            .expect("test operation should succeed");
        let b = ObservedInputManifest::observe([("a", b"one".to_vec()), ("b", b"two".to_vec())])
            .expect("test operation should succeed");
        assert_eq!(a, b);
        assert_eq!(cache_key("v1", b"request", &a), cache_key("v1", b"request", &b));
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn secrets_are_rejected_before_digesting() {
        assert!(ObservedInputManifest::observe([(".env", b"API_KEY=x".to_vec())]).is_err());
        assert!(ObservedInputManifest::observe([("readme", b"ordinary text".to_vec())]).is_ok());
        assert!(contains_secret("notes", b"password=hidden"));
        assert_eq!(
            redact_secrets("api_key=hidden\nordinary"),
            "api_key=<REDACTED>\nordinary"
        );
    }

    #[test]
    fn bare_bearer_and_key_json_credentials_are_rejected_and_redacted() {
        assert!(contains_secret(
            "notes",
            b"Authorization: Bearer sk-ant-abcdef123\nrest"
        ));
        assert!(contains_secret("notes", b"Bearer sk-ant-abcdef123"));
        assert!(contains_secret("notes", br#"{"key": "sk-abc123xyz"}"#));
        assert!(contains_secret("notes", b"sk-abc123xyz"));
        assert!(!contains_secret("notes", b"task-123 review the risk-item"));
        assert!(!contains_secret("notes", b"ordinary text"));
        assert_eq!(
            redact_secrets("Bearer sk-ant-abcdef\nkeep"),
            "Bearer <REDACTED>\nkeep"
        );
        assert_eq!(redact_secrets("note sk-abc123 ok"), "note sk-<REDACTED> ok");
        assert_eq!(redact_secrets("task-123 fine"), "task-123 fine");
    }

    #[test]
    fn hot_cache_tracks_lru_hits_bytes_and_fresh_bypass() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let mut cache = HotCache::new(5);
        cache.insert(entry("a", CacheClass::Exact, 3, now));
        cache.insert(entry("b", CacheClass::Ttl, 2, now));
        assert_eq!(
            cache.get("a", now, CachePolicy::default(), LookupMode::FreshOnly),
            Some(&[0, 0, 0][..])
        );
        assert!(
            cache
                .get("a", now, CachePolicy::default(), LookupMode::Bypass)
                .is_none()
        );
        cache.insert(entry("c", CacheClass::Exact, 4, now));
        assert!(
            cache
                .get("b", now, CachePolicy::default(), LookupMode::FreshOnly)
                .is_none()
        );
        assert_eq!(cache.bytes(), 4);
        assert_eq!(cache.metrics().hits, 1);
    }

    #[test]
    fn hot_cache_enforces_entry_and_byte_limits_with_true_lru_eviction() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let mut cache = HotCache::with_limits(2, 10);
        cache.insert(entry("a", CacheClass::Exact, 3, now));
        cache.insert(entry("b", CacheClass::Exact, 3, now));
        assert!(
            cache
                .get("a", now, CachePolicy::default(), LookupMode::AllowStale)
                .is_some()
        );
        cache.insert(entry("c", CacheClass::Exact, 3, now));
        assert!(
            cache
                .get("a", now, CachePolicy::default(), LookupMode::AllowStale)
                .is_some()
        );
        assert!(
            cache
                .get("b", now, CachePolicy::default(), LookupMode::AllowStale)
                .is_none()
        );
        assert!(
            cache
                .get("c", now, CachePolicy::default(), LookupMode::AllowStale)
                .is_some()
        );

        let mut byte_limited = HotCache::with_limits(10, 5);
        byte_limited.insert(entry("a", CacheClass::Exact, 3, now));
        byte_limited.insert(entry("b", CacheClass::Exact, 2, now));
        byte_limited.insert(entry("c", CacheClass::Exact, 4, now));
        assert_eq!(byte_limited.bytes(), 4);
        assert!(
            byte_limited
                .get("a", now, CachePolicy::default(), LookupMode::AllowStale)
                .is_none()
        );
        assert!(
            byte_limited
                .get("b", now, CachePolicy::default(), LookupMode::AllowStale)
                .is_none()
        );
        assert!(
            byte_limited
                .get("c", now, CachePolicy::default(), LookupMode::AllowStale)
                .is_some()
        );
    }

    #[test]
    fn pinned_entries_are_not_evicted_when_limits_are_exceeded() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let mut cache = HotCache::with_limits(1, 10);
        let mut pinned = entry("pinned", CacheClass::Exact, 3, now);
        pinned.pinned = true;
        cache.insert(pinned);
        cache.insert(entry("ordinary", CacheClass::Exact, 3, now));
        assert!(
            cache
                .get("pinned", now, CachePolicy::default(), LookupMode::AllowStale)
                .is_some()
        );
        assert!(
            cache
                .get("ordinary", now, CachePolicy::default(), LookupMode::AllowStale)
                .is_none()
        );
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn prune_never_selects_pinned_and_prefers_stale() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let mut cache = HotCache::new(20);
        cache.insert(entry("fresh", CacheClass::Exact, 4, now));
        cache.insert(entry("stale", CacheClass::Ttl, 4, now - Duration::from_secs(1)));
        cache
            .entries
            .get_mut("stale")
            .expect("test operation should succeed")
            .pinned = true;
        assert_eq!(
            cache.prune_selection(
                now + Duration::from_secs(30 * 24 * 60 * 60),
                CachePolicy::default(),
                4
            ),
            vec!["fresh"]
        );
    }
}

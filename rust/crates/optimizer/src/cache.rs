use repeaternet_rf_core::{HeightReference, LinkResult, RfError, Site, SiteKind};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheStats {
    pub hits: usize,
    pub misses: usize,
    pub evictions: usize,
    pub entries: usize,
    pub capacity: usize,
}

/// Shared bounded LRU for scalar RF metrics. Terrain and radio revision
/// identifiers are supplied by the caller with each use.
pub struct LinkMetricsCache {
    capacity: usize,
    state: Mutex<CacheState>,
    finished: Condvar,
}

impl Default for LinkMetricsCache {
    fn default() -> Self {
        Self::new(50_000)
    }
}

#[derive(Default)]
struct CacheState {
    tick: u64,
    entries: HashMap<CacheKey, (u64, LinkResult)>,
    lru: BTreeSet<(u64, CacheKey)>,
    stats: HashMap<String, CacheStats>,
    inflight: HashSet<CacheKey>,
}

struct Flight<'a> {
    cache: &'a LinkMetricsCache,
    key: CacheKey,
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        let mut state = self.cache.state.lock().unwrap_or_else(|p| p.into_inner());
        state.inflight.remove(&self.key);
        self.cache.finished.notify_all();
    }
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct CacheKey {
    namespace: String,
    revision: String,
    source: SiteKey,
    target: SiteKey,
    sample_step: u64,
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct SiteKey {
    id: String,
    kind: u8,
    reference: u8,
    geometry: [u64; 4],
    surface: Option<u64>,
}

impl From<&Site> for SiteKey {
    fn from(site: &Site) -> Self {
        let kind = match site.kind {
            SiteKind::EndpointA => 0,
            SiteKind::EndpointB => 1,
            SiteKind::Client => 2,
            SiteKind::Router => 3,
            SiteKind::Candidate => 4,
        };
        Self {
            id: site.id.clone(),
            kind,
            reference: u8::from(site.height_reference == HeightReference::SurfaceDom),
            geometry: [
                site.position.x,
                site.position.y,
                site.ground_elevation_m,
                site.antenna_height_m,
            ]
            .map(f64::to_bits),
            surface: site.surface_elevation_m.map(f64::to_bits),
        }
    }
}

impl CacheKey {
    pub(crate) fn new(
        namespace: &str,
        revision: &str,
        source: &Site,
        target: &Site,
        step: f64,
    ) -> Self {
        Self {
            namespace: namespace.into(),
            revision: revision.into(),
            source: source.into(),
            target: target.into(),
            sample_step: step.to_bits(),
        }
    }
}

impl LinkMetricsCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(CacheState::default()),
            finished: Condvar::new(),
        }
    }

    pub fn stats(&self, namespace: &str) -> CacheStats {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        CacheStats {
            capacity: self.capacity,
            ..state.stats.get(namespace).copied().unwrap_or_default()
        }
    }

    /// Clear one namespace and reset its counters. Call when jobs in that namespace are idle.
    pub fn clear(&self, namespace: &str) -> usize {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.lru.retain(|(_, key)| key.namespace != namespace);
        state.entries.retain(|key, _| key.namespace != namespace);
        state
            .stats
            .remove(namespace)
            .map_or(0, |stats| stats.entries)
    }

    pub(crate) fn get_or_compute(
        &self,
        key: CacheKey,
        cancelled: &dyn Fn() -> bool,
        compute: impl FnOnce() -> Result<Option<LinkResult>, RfError>,
    ) -> Result<Option<LinkResult>, RfError> {
        let flight = loop {
            if cancelled() {
                return Ok(None);
            }
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.tick += 1;
            let tick = state.tick;
            if let Some((last, link)) = state.entries.get_mut(&key) {
                let (previous, link) = (*last, link.clone());
                *last = tick;
                state.lru.remove(&(previous, key.clone()));
                state.lru.insert((tick, key.clone()));
                state.stats.entry(key.namespace.clone()).or_default().hits += 1;
                return Ok(Some(link));
            }
            if self.capacity == 0 || state.inflight.insert(key.clone()) {
                state.stats.entry(key.namespace.clone()).or_default().misses += 1;
                if self.capacity == 0 {
                    drop(state);
                    return compute();
                }
                break Flight { cache: self, key };
            }
            // A waiting job can stop even while the owning job is still sampling.
            drop(self.finished.wait_timeout(state, Duration::from_millis(10)));
        };
        let link = compute()?;
        if let Some(link) = &link {
            self.put(flight.key.clone(), link.clone());
        }
        Ok(link)
    }

    fn put(&self, key: CacheKey, link: LinkResult) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.entries.len() == self.capacity {
            if let Some((_, old)) = state.lru.pop_first() {
                state.entries.remove(&old);
                let stats = state.stats.entry(old.namespace).or_default();
                stats.entries -= 1;
                stats.evictions += 1;
            }
        }
        state.tick += 1;
        let tick = state.tick;
        state.lru.insert((tick, key.clone()));
        state
            .stats
            .entry(key.namespace.clone())
            .or_default()
            .entries += 1;
        state.entries.insert(key, (tick, link));
    }
}

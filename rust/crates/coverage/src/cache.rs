use std::{
    collections::HashSet,
    num::NonZeroUsize,
    sync::{Condvar, Mutex},
    time::Duration,
};

use lru::LruCache;
use repeaternet_rf_core::{
    AntennaPattern, HeightReference, LinkResult, RadioBudget, RfSettings, Site, TerrainSource,
    ValidationMode,
};
use sha2::{Digest, Sha256};

use crate::{invalid, CoverageError, CoverageSourceResult, Rejection};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CoverageCacheStats {
    pub hits: usize,
    pub misses: usize,
    pub evictions: usize,
    pub entries: usize,
    pub capacity: usize,
}

/// Process-local LRU for successful scalar coverage link evaluations.
pub struct CoverageMetricsCache {
    state: Mutex<State>,
    finished: Condvar,
}

struct State {
    entries: LruCache<Key, Metrics>,
    inflight: HashSet<Key>,
    stats: CoverageCacheStats,
}

pub(crate) struct CacheContext<'a> {
    pub cache: &'a CoverageMetricsCache,
    revision: [u8; 32],
}

impl<'a> CacheContext<'a> {
    pub fn new(
        cache: &'a CoverageMetricsCache,
        terrain_revision: &[u8; 32],
        terrain: &(impl TerrainSource + ?Sized),
        rf: &RfSettings,
    ) -> Self {
        let mut digest = Sha256::new();
        digest.update(terrain_revision);
        digest.update(terrain.resolution_m().to_bits().to_le_bytes());
        digest.update([u8::from(terrain.has_surface())]);
        // The typed settings' derived Debug includes the loaded antenna samples
        // and LoRa thresholds. This identity is used only within one process.
        digest.update(format!("{rf:?}"));
        Self {
            cache,
            revision: digest.finalize().into(),
        }
    }

    pub fn key(&self, source: &Site, target: &Site, step: f64, radio: RadioKey) -> Key {
        Key {
            revision: self.revision,
            source: source.into(),
            target: target.into(),
            step: step.to_bits(),
            radio,
        }
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(crate) struct Key {
    revision: [u8; 32],
    source: SiteKey,
    target: SiteKey,
    step: u64,
    radio: RadioKey,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct SiteKey {
    geometry: [u64; 5],
    flags: [u8; 3],
}

impl From<&Site> for SiteKey {
    fn from(site: &Site) -> Self {
        // Labels only affect the output. Reattach the current source ID at the
        // call site so overlapping grids can reuse links with different indices.
        Self {
            geometry: [
                site.position.x,
                site.position.y,
                site.ground_elevation_m,
                site.surface_elevation_m.unwrap_or_default(),
                site.antenna_height_m,
            ]
            .map(f64::to_bits),
            flags: [
                site.kind as u8,
                u8::from(site.height_reference == HeightReference::SurfaceDom),
                u8::from(site.surface_elevation_m.is_some()),
            ],
        }
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(crate) enum RadioKey {
    Defaults,
    Client([u64; 5]),
}

impl RadioKey {
    pub fn new(radio: Option<&RadioBudget>) -> Option<Self> {
        let Some(radio) = radio else {
            return Some(Self::Defaults);
        };
        let AntennaPattern::ConstantGain(gain) = radio.antenna.pattern else {
            return None;
        };
        // A radio override's nominal height is already represented by the site.
        Some(Self::Client(
            [
                gain,
                radio.antenna.feed_loss_db,
                radio.tx_power_dbm,
                radio.sensitivity_dbm,
                radio.miscellaneous_loss_db,
            ]
            .map(f64::to_bits),
        ))
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Metrics {
    down: f64,
    up: f64,
    forward_valid: bool,
    reverse_valid: bool,
    structural_ok: bool,
    pub valid: bool,
}

impl Metrics {
    pub fn new(link: &LinkResult, mode: ValidationMode) -> Self {
        Self {
            down: link.forward.usable_margin_db,
            up: link.reverse.usable_margin_db,
            forward_valid: link.forward.valid,
            reverse_valid: link.reverse.valid,
            structural_ok: mode != ValidationMode::StrictLos
                || (link.los_clear && link.fresnel_clear),
            valid: link.valid,
        }
    }

    fn is_finite(self) -> bool {
        self.down.is_finite() && self.up.is_finite()
    }

    pub fn write_source(self, result: &mut CoverageSourceResult) -> Result<(), CoverageError> {
        if !self.is_finite() {
            return Err(invalid("Coverage radio margins must be finite"));
        }
        result.downlink_margin_db = Some(self.down);
        result.uplink_margin_db = Some(self.up);
        result.two_way_margin_db = Some(self.down.min(self.up));
        result.valid_downlink = self.structural_ok && self.forward_valid;
        result.valid_uplink = self.structural_ok && self.reverse_valid;
        result.valid_two_way = result.valid_downlink && result.valid_uplink;
        result.rejection = (!self.structural_ok).then_some(Rejection::Validation);
        Ok(())
    }
}

struct Flight<'a> {
    cache: &'a CoverageMetricsCache,
    key: Key,
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        let mut state = self.cache.state.lock().unwrap_or_else(|p| p.into_inner());
        state.inflight.remove(&self.key);
        self.cache.finished.notify_all();
    }
}

impl CoverageMetricsCache {
    pub fn new(capacity: usize) -> Self {
        let capacity = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::MIN);
        Self {
            state: Mutex::new(State {
                entries: LruCache::sparse(capacity),
                inflight: HashSet::new(),
                stats: CoverageCacheStats {
                    capacity: capacity.get(),
                    ..CoverageCacheStats::default()
                },
            }),
            finished: Condvar::new(),
        }
    }

    pub fn stats(&self) -> CoverageCacheStats {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        CoverageCacheStats {
            entries: state.entries.len(),
            ..state.stats
        }
    }

    pub(crate) fn get_or_compute(
        &self,
        key: Key,
        cancelled: &dyn Fn() -> bool,
        compute: impl FnOnce() -> Result<Option<Metrics>, CoverageError>,
    ) -> Result<Option<Metrics>, CoverageError> {
        let flight = loop {
            check_cancelled(cancelled)?;
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(metrics) = state.entries.get(&key).copied() {
                state.stats.hits += 1;
                return Ok(Some(metrics));
            }
            if state.inflight.contains(&key) {
                // A waiter can stop independently of the job reading terrain.
                drop(self.finished.wait_timeout(state, Duration::from_millis(10)));
                continue;
            }
            state.stats.misses += 1;
            // Bound bookkeeping as well as stored values. An unrelated link
            // can compute without the cache if all flight slots are occupied.
            if state.inflight.len() == state.stats.capacity {
                break None;
            }
            state.inflight.insert(key);
            break Some(Flight { cache: self, key });
        };
        let result = compute();
        check_cancelled(cancelled)?;
        let result = result?;
        if let (Some(_flight), Some(metrics)) = (&flight, result.filter(|value| value.is_finite()))
        {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.stats.evictions += usize::from(state.entries.push(key, metrics).is_some());
        }
        Ok(result)
    }
}

pub(crate) fn check_cancelled(cancelled: &dyn Fn() -> bool) -> Result<(), CoverageError> {
    if cancelled() {
        return Err(CoverageError::Cancelled);
    }
    Ok(())
}

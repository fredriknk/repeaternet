//! Evaluate each directed link on stored terrain and publish sorted JSONL.

use std::{collections::HashMap, path::Path, thread, time::Instant};

use anyhow::{Context, Result, ensure};
use repeaternet_rf_core::{
    EvaluationOptions, LinkEvaluator, RfSettings, Site, free_space_path_loss_db,
};
use serde_json::{Value, json};

use crate::{
    dataset::{LinkRecord, PathFeatures, PredictionRecord, ProfileRecord, SCHEMA_VERSION},
    engines::{
        self, Clearance, DELTA, DEYGOUT, EPSTEIN_PETERSON, GIOVANELI, IDS, RFCORE, StoredTerrain,
        clutter, delta, multi_edge,
    },
    jsonl::{self, AtomicWriter},
    paths,
};

const HEIGHTS_M: [f64; 4] = [2.0, 5.0, 10.0, 20.0];
const LINKS_PER_BATCH: usize = 32;

struct Input<'a> {
    link: &'a LinkRecord,
    profile: &'a ProfileRecord,
    features: &'a PathFeatures,
}

struct Scenario<'a> {
    input: &'a Input<'a>,
    tx: &'a Site,
    rx: &'a Site,
    freq: f64,
    k: f64,
    step: f64,
    fspl: f64,
}

impl Scenario<'_> {
    fn row(
        &self,
        engine: &str,
        surface: &str,
        excess: f64,
        geometry: &Clearance,
    ) -> PredictionRecord {
        PredictionRecord {
            schema: SCHEMA_VERSION,
            link_id: self.input.link.link_id.clone(),
            path_id: self.input.profile.path_id.clone(),
            engine: engine.into(),
            surface: surface.into(),
            clutter: "none".into(),
            tx_height_m: self.tx.antenna_height_m,
            rx_height_m: self.rx.antenna_height_m,
            k_factor: self.k,
            step_m: self.step,
            freq_mhz: self.freq,
            fspl_db: self.fspl,
            excess_db: excess,
            clutter_db: 0.0,
            total_db: self.fspl + excess,
            v_max: Some(geometry.direct_v),
            line_of_sight: Some(geometry.los),
            diag: Value::Null,
        }
    }

    fn terrain_rows(&self, surface: bool, rows: &mut Vec<PredictionRecord>) -> Result<Option<f64>> {
        let profile = self.input.profile;
        let terrain = StoredTerrain { profile, surface };
        let rf = RfSettings {
            frequency_mhz: self.freq,
            k_factor: self.k,
            clutter_loss_enabled: surface,
            ..RfSettings::default()
        };
        let result = LinkEvaluator::new(&terrain, &rf).evaluate(
            self.tx,
            self.rx,
            EvaluationOptions {
                sample_step_m: Some(self.step),
                ..EvaluationOptions::default()
            },
        )?;
        let sampled = result
            .profile
            .as_ref()
            .context("missing sampled RF geometry")?;
        let geometry = Clearance::from_profile(sampled, self.freq);
        let label = if surface { "dom" } else { "dtm" };
        let mut row = self.row(RFCORE, label, result.diffraction_loss_db, &geometry);
        if let Some(edge) = result.dominant_obstacles.first() {
            row.v_max = Some(edge.v);
        }
        row.diag = engines::bullington_diag(&result.dominant_obstacles);
        rows.push(row);

        if self.step == profile.resolution_m && self.k == 1.0 {
            let delta = delta::evaluate(sampled, self.freq, self.k, result.diffraction_loss_db)?;
            let mut row = self.row(DELTA, label, delta.loss, &geometry);
            if let Some(edge) = result.dominant_obstacles.first() {
                row.v_max = Some(edge.v);
            }
            row.diag = engines::bullington_diag(&result.dominant_obstacles);
            row.diag["lbull_db"] = json!(result.diffraction_loss_db);
            row.diag["lbs_db"] = json!(delta.smooth_bullington);
            row.diag["lsph_db"] = json!(delta.spherical);
            row.diag["smooth_agl_m"] = json!(delta.smooth_agl);
            rows.push(row);

            let mut z = sampled.effective_obstruction_m.clone();
            let last = z.len() - 1;
            z[0] = self.tx.antenna_absolute_elevation_m();
            z[last] = self.rx.antenna_absolute_elevation_m();
            let multi = multi_edge::evaluate(&sampled.distances_m, &z, self.freq);
            let diag = json!({ "edges": multi.edges });
            for (id, loss) in [
                (DEYGOUT, multi.deygout),
                (GIOVANELI, multi.giovaneli),
                (EPSTEIN_PETERSON, multi.epstein_peterson),
            ] {
                let mut row = self.row(id, label, loss, &geometry);
                row.diag = diag.clone();
                rows.push(row);
            }
        }
        Ok(surface.then_some(result.forward.clutter_loss_db))
    }
}

fn add_clutter(row: &PredictionRecord, model: &str, loss: f64) -> PredictionRecord {
    let mut variant = row.clone();
    variant.clutter = model.into();
    variant.clutter_db = loss;
    variant.total_db += loss;
    variant
}

fn for_link(input: &Input<'_>) -> Result<Vec<PredictionRecord>> {
    let profile = input.profile;
    let freq = input.link.radio.freq_mhz.unwrap_or(869.5);
    let reverse = input.link.tx.node == profile.b.node;
    let a = (&profile.a, 0.0);
    let b = (&profile.b, profile.distance_m);
    let [(from, tx_m), (to, rx_m)] = if reverse { [b, a] } else { [a, b] };
    ensure!(
        input.link.tx.node == from.node && input.link.rx.node == to.node,
        "link endpoints disagree with the profile"
    );
    let mut output = Vec::new();
    for tx_height in HEIGHTS_M {
        for rx_height in HEIGHTS_M {
            let tx = engines::site(from, tx_m, tx_height);
            let rx = engines::site(to, rx_m, rx_height);
            let separation = profile
                .distance_m
                .hypot(rx.antenna_absolute_elevation_m() - tx.antenna_absolute_elevation_m());
            let fspl = free_space_path_loss_db(separation, freq)?;
            for k in [1.0, 4.0 / 3.0] {
                let terminal = clutter::evaluate(profile, input.features, &tx, &rx, freq, k);
                for step in [profile.resolution_m, 50.0, 200.0] {
                    let scenario = Scenario {
                        input,
                        tx: &tx,
                        rx: &rx,
                        freq,
                        k,
                        step,
                        fspl,
                    };
                    let start = output.len();
                    scenario.terrain_rows(false, &mut output)?;
                    let dtm_end = output.len();
                    let rfcore_clutter = if profile.dom.is_some() {
                        scenario
                            .terrain_rows(true, &mut output)?
                            .unwrap_or_default()
                    } else {
                        0.0
                    };
                    for i in start..dtm_end {
                        let dtm = &output[i];
                        let mut variants = Vec::new();
                        if dtm.engine == RFCORE {
                            variants.push(add_clutter(dtm, "rfcore_clutter", rfcore_clutter));
                        }
                        if let Some(terminal) = &terminal {
                            if let Some(loss) = terminal.p2108_db {
                                let mut row = add_clutter(dtm, "dom_p2108", loss);
                                row.diag["terminals"] = json!(terminal.terminals);
                                variants.push(row);
                            }
                            let mut row = add_clutter(dtm, "p833", terminal.p833_db);
                            row.diag["terminals"] = json!(terminal.terminals);
                            variants.push(row);
                        }
                        output.extend(variants);
                    }
                }
            }
        }
    }
    output.sort_unstable_by(|a, b| {
        a.surface
            .cmp(&b.surface)
            .then(a.clutter.cmp(&b.clutter))
            .then(a.tx_height_m.total_cmp(&b.tx_height_m))
            .then(a.rx_height_m.total_cmp(&b.rx_height_m))
            .then(a.k_factor.total_cmp(&b.k_factor))
            .then(a.step_m.total_cmp(&b.step_m))
    });
    Ok(output)
}

fn validate_profile(profile: &ProfileRecord, features: &PathFeatures) -> Result<()> {
    ensure!(
        profile.schema == SCHEMA_VERSION && features.schema == SCHEMA_VERSION,
        "unsupported schema"
    );
    ensure!(
        profile.distance_m.is_finite()
            && profile.distance_m > 0.0
            && [10.0, 30.0].contains(&profile.resolution_m)
            && profile.step_m.is_finite()
            && profile.step_m > 0.0,
        "invalid profile grid"
    );
    let count = ((profile.distance_m / profile.resolution_m).ceil() as usize)
        .max(2)
        .saturating_add(1);
    ensure!(
        profile.dtm.len() == count
            && (profile.distance_m / (count - 1) as f64 - profile.step_m).abs() < 1e-9,
        "DTM samples do not match the profile grid"
    );
    ensure!(
        profile.dom.as_ref().is_none_or(|dom| dom.len() == count),
        "DOM samples do not match the profile grid"
    );
    ensure!(
        profile.a.ground_m == profile.dtm[0] && profile.b.ground_m == profile.dtm[count - 1],
        "terminal ground elevations disagree with the profile"
    );
    ensure!(
        features.distance_m == profile.distance_m,
        "path feature distance disagrees with the profile"
    );
    ensure!(
        profile.dom.is_none()
            || (features.a_clutter_height_m.is_some() && features.b_clutter_height_m.is_some()),
        "missing terminal clutter heights for DOM profile"
    );
    Ok(())
}

pub fn run(links: &Path, work: &Path) -> Result<()> {
    let started = Instant::now();
    paths::refuse_user_data(links)?;
    for name in ["profiles.jsonl", "path_features.jsonl", "predictions"] {
        paths::refuse_user_data(&work.join(name))?;
    }
    let mut links = jsonl::read::<LinkRecord>(links)?;
    links.sort_unstable_by(|a, b| a.link_id.cmp(&b.link_id));
    ensure!(
        links
            .windows(2)
            .all(|pair| pair[0].link_id != pair[1].link_id),
        "duplicate link_id in links"
    );
    let profiles = jsonl::read::<ProfileRecord>(&work.join("profiles.jsonl"))?;
    let features = jsonl::read::<PathFeatures>(&work.join("path_features.jsonl"))?;
    let mut by_path = HashMap::new();
    for features in &features {
        ensure!(
            by_path
                .insert(features.path_id.as_str(), features)
                .is_none(),
            "duplicate path_id in path_features"
        );
    }
    let mut by_link = HashMap::new();
    for profile in &profiles {
        let features = by_path
            .get(profile.path_id.as_str())
            .with_context(|| format!("missing features for {}", profile.path_id))?;
        validate_profile(profile, features)
            .with_context(|| format!("profile {}", profile.path_id))?;
        for id in &profile.link_ids {
            ensure!(
                by_link.insert(id.as_str(), (profile, *features)).is_none(),
                "more than one profile for link {id}"
            );
        }
    }
    let inputs: Vec<_> = links
        .iter()
        .filter_map(|link| {
            let &(profile, features) = by_link.get(link.link_id.as_str())?;
            Some(Input {
                link,
                profile,
                features,
            })
        })
        .collect();
    let mut files = IDS
        .iter()
        .map(|id| {
            let path = work.join("predictions").join(format!("{id}.jsonl"));
            Ok((AtomicWriter::new(&path)?, path))
        })
        .collect::<Result<Vec<_>>>()?;
    let workers = thread::available_parallelism().map_or(1, usize::from);
    let mut counts = [0; IDS.len()];
    for batch in inputs.chunks(LINKS_PER_BATCH) {
        let results = thread::scope(|scope| {
            let handles: Vec<_> = batch
                .chunks(batch.len().div_ceil(workers))
                .map(|chunk| {
                    scope.spawn(move || {
                        chunk
                            .iter()
                            .map(|input| {
                                for_link(input)
                                    .with_context(|| format!("link {}", input.link.link_id))
                            })
                            .collect::<Result<Vec<_>>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("prediction worker panicked"))
                .collect::<Result<Vec<_>>>()
        })?;
        for row in results.iter().flatten().flatten() {
            let index = IDS.iter().position(|id| *id == row.engine).unwrap();
            files[index].0.push(row)?;
            counts[index] += 1;
        }
    }
    for (((writer, path), id), count) in files.into_iter().zip(IDS).zip(counts) {
        writer.publish(&path)?;
        println!("Wrote {count} {id} predictions");
    }
    println!(
        "Evaluated {} directed links ({} without profiles) in {:.2}s",
        inputs.len(),
        links.len() - inputs.len(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

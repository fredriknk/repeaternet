//! Exhaustive link screening and exact two-terminal route search.

use super::{
    Cancellation, OptimizationError, OptimizationResult, Pair, RoutingGraph, Search, Stage,
};
use crate::parallel::for_each_unordered;
use crate::paths::{plan_routes, LinkGraph, Screened};
use crate::{pair, NetworkSolution};
use repeaternet_rf_core::LinkResult;
use std::collections::{BTreeMap, BTreeSet};

/// Coarse rejections within this many dB of passing are rechecked at final resolution.
const RETRY_BAND_DB: f64 = 3.0;
/// Coarse clearance failures within this many metres of grazing are rechecked.
const RETRY_CLEARANCE_M: f64 = 5.0;
/// Plan and certify rounds before the search falls back to certified links only.
const MAX_CERTIFICATION_ROUNDS: usize = 400;

fn worth_retrying(link: Option<&LinkResult>) -> bool {
    let Some(link) = link else {
        // Missing terrain at coarse resolution.
        return true;
    };
    let budget_ok = link.forward.valid && link.reverse.valid;
    if budget_ok {
        // Failed only on clearance in a strict validation mode.
        link.minimum_clearance_m >= -RETRY_CLEARANCE_M
    } else {
        link.worst_margin_db() >= -RETRY_BAND_DB
    }
}

impl Search<'_, '_> {
    pub(super) fn rf_workers(&self, len: usize) -> usize {
        if !self.rf.builtin {
            return 1;
        }
        let requested = if self.settings.parallel_workers == 0 {
            std::thread::available_parallelism()
                .map_or(1, |cpus| cpus.get().saturating_sub(1).max(1))
        } else {
            self.settings.parallel_workers
        };
        requested.min(len).max(1)
    }

    /// Certify pairs at final resolution in parallel. Results are stored by key, so
    /// the outcome does not depend on completion order.
    pub(super) fn certify_unordered(
        &mut self,
        pairs: &[Pair],
        stage: Stage,
    ) -> Result<(), OptimizationError> {
        let pairs: Vec<Pair> = pairs
            .iter()
            .copied()
            .filter(|key| !self.final_checked.contains(key))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if pairs.is_empty() || self.hooks.cancelled() {
            return Ok(());
        }
        let total = pairs.len();
        self.hooks.notify(stage, 0, total);
        let workers = self.rf_workers(total);
        let cancellation = self.hooks.cancellation;
        let cancelled = || cancellation.is_some_and(|poll| poll() != Cancellation::Continue);
        let (rf, sites, hooks) = (&self.rf, &self.sites, &mut self.hooks);
        let (links, checked) = (&mut self.links, &mut self.final_checked);
        let max_distance = self.settings.maximum_link_distance_m;
        let step = self.settings.final_sample_step_m;
        let mut done = 0;
        for_each_unordered(
            &pairs,
            workers,
            &cancelled,
            |key| {
                if !rf.plausible(sites, key, max_distance)? {
                    return Ok::<_, OptimizationError>(None);
                }
                Ok(rf
                    .search_pair(sites, key, step, false, &|| false)?
                    .filter(|link| link.valid))
            },
            |key, link| {
                Self::store_certification(links, checked, key, link, true);
                done += 1;
                if done % 16 == 0 || done == total {
                    hooks.notify(stage, done, total);
                }
            },
        )
    }

    /// Evaluate every pair within link range that passes the free-space screen.
    /// Terminal links are certified directly at final resolution; all other
    /// links are screened at coarse resolution with a narrow final retry band.
    ///
    /// With `compact`, coarse passes are kept only as scalar summaries in
    /// `self.coarse`; otherwise they enter `self.links` for the general solver.
    pub(super) fn exhaustive_screen(
        &mut self,
        terminals: &BTreeSet<usize>,
        compact: bool,
    ) -> Result<(), OptimizationError> {
        let count = self.sites.len();
        self.hooks.notify(Stage::ScreeningPossibleLinks, 0, count);
        let mut terminal_pairs = Vec::new();
        let mut pairs = Vec::new();
        for left in 0..count {
            if self.hooks.cancelled() {
                return Ok(());
            }
            for right in left + 1..count {
                let key = (left, right);
                if self.final_checked.contains(&key) || self.links.contains_key(&key) {
                    continue;
                }
                if !self.plausible(key)? {
                    continue;
                }
                if terminals.contains(&left) || terminals.contains(&right) {
                    terminal_pairs.push(key);
                } else {
                    pairs.push(key);
                }
            }
            if (left + 1) % 64 == 0 || left + 1 == count {
                self.hooks
                    .notify(Stage::ScreeningPossibleLinks, left + 1, count);
            }
        }
        self.certify_unordered(&terminal_pairs, Stage::CheckingEndpointVisibleCandidates)?;
        if self.hooks.cancelled() {
            return Ok(());
        }
        let total = pairs.len();
        self.hooks.notify(Stage::EvaluatingRfLinks, 0, total);
        let workers = self.rf_workers(total);
        let cancellation = self.hooks.cancellation;
        let cancelled = || cancellation.is_some_and(|poll| poll() != Cancellation::Continue);
        let (rf, sites, hooks, links) = (&self.rf, &self.sites, &mut self.hooks, &mut self.links);
        let coarse = &mut self.coarse;
        let step = self.settings.coarse_sample_step_m;
        let mut retry = Vec::new();
        let mut passed = 0;
        let mut done = 0;
        for_each_unordered(
            &pairs,
            workers,
            &cancelled,
            |key| rf.screen_pair(sites, key, step),
            |key, link| {
                match link {
                    Some(link) if link.valid => {
                        passed += 1;
                        if compact {
                            coarse.push((key, Screened::from(&link)));
                        } else {
                            links.insert(key, link);
                        }
                    }
                    link if worth_retrying(link.as_ref()) => retry.push(key),
                    _ => {}
                }
                done += 1;
                if done % 512 == 0 || done == total {
                    hooks.notify(Stage::EvaluatingRfLinks, done, total);
                }
            },
        )?;
        if self.hooks.cancelled() {
            return Ok(());
        }
        self.diagnostics.push(format!(
            "Screened {total} candidate links within range; {passed} passed at coarse resolution and {} near-misses were rechecked at final resolution.",
            retry.len()
        ));
        self.certify_unordered(&retry, Stage::EvaluatingRfLinks)
    }

    fn terminal_indices(&self) -> (usize, usize, Vec<usize>) {
        let first = self
            .sites
            .iter()
            .position(|site| site.is_client())
            .expect("two clients");
        let last = self
            .sites
            .iter()
            .rposition(|site| site.is_client())
            .expect("two clients");
        let required = (0..self.sites.len())
            .filter(|&i| self.sites[i].is_required())
            .collect();
        (first, last, required)
    }

    fn link_graph<'l>(
        &self,
        links: impl IntoIterator<Item = (&'l Pair, &'l LinkResult)>,
    ) -> LinkGraph {
        LinkGraph::new(
            self.sites.iter().map(|site| site.rf.position).collect(),
            links
                .into_iter()
                .filter(|(_, link)| link.valid)
                .map(|(&key, link)| (key, Screened::from(link))),
        )
    }

    /// Coarse summaries for unchecked pairs plus every certified link.
    fn screening_graph(&self) -> LinkGraph {
        let coarse = self
            .coarse
            .iter()
            .filter(|(key, _)| !self.final_checked.contains(key) && !self.links.contains_key(key))
            .copied();
        let links = self
            .links
            .iter()
            .filter(|(_, link)| link.valid)
            .map(|(&key, link)| (key, Screened::from(link)));
        LinkGraph::new(
            self.sites.iter().map(|site| site.rf.position).collect(),
            coarse.chain(links),
        )
    }

    /// Plan routes over site indices.
    fn plan_on(&self, graph: &LinkGraph) -> Vec<Vec<usize>> {
        let (first, last, required) = self.terminal_indices();
        plan_routes(
            graph,
            first,
            last,
            &required,
            self.settings.maximum_solution_routers,
            &|| self.hooks.cancelled(),
        )
        .into_iter()
        .map(|planned| planned.nodes)
        .collect()
    }

    /// Plan on coarse and certified links, certify the hops the plan uses, and
    /// repeat until every planned hop is certified at final resolution.
    fn certified_plans(&mut self) -> Result<(Vec<Vec<usize>>, usize), OptimizationError> {
        let mut graph = self.screening_graph();
        let mut rounds = 0;
        while rounds < MAX_CERTIFICATION_ROUNDS {
            if self.hooks.cancelled() {
                return Ok((Vec::new(), rounds));
            }
            let pending: Vec<Pair> = self
                .plan_on(&graph)
                .iter()
                .flat_map(|path| path.windows(2).map(|hop| pair(hop[0], hop[1])))
                .filter(|key| !self.final_checked.contains(key))
                .collect();
            if pending.is_empty() {
                break;
            }
            rounds += 1;
            self.hooks
                .notify(Stage::ValidatingSolutionAlternatives, rounds, rounds + 1);
            self.certify_unordered(&pending, Stage::ValidatingSolutionAlternatives)?;
            for (left, right) in pending {
                graph.set(left, right, self.links.get(&(left, right)));
            }
        }
        let certified = self.link_graph(
            self.links
                .iter()
                .filter(|(key, _)| self.final_checked.contains(key)),
        );
        Ok((self.plan_on(&certified), rounds))
    }

    /// Certify every pair among each route's sites and build the installations.
    fn installations(
        &mut self,
        plans: &[Vec<usize>],
        terminals: &BTreeSet<usize>,
        rounds: usize,
    ) -> Result<Vec<NetworkSolution>, OptimizationError> {
        let extra: Vec<Pair> = plans
            .iter()
            .flat_map(|path| {
                path.iter().enumerate().flat_map(move |(i, &left)| {
                    path[i + 1..].iter().map(move |&right| pair(left, right))
                })
            })
            .collect();
        self.certify_unordered(&extra, Stage::ValidatingSolutionAlternatives)?;
        let graph = RoutingGraph::from_links(
            &self.sites,
            self.links
                .iter()
                .filter(|(key, _)| self.final_checked.contains(key))
                .map(|(_, link)| link),
        )?;
        let positions: BTreeMap<&str, usize> = graph
            .sites
            .iter()
            .enumerate()
            .map(|(i, site)| (site.rf.id.as_str(), i))
            .collect();
        let screened = self.screened_links;
        let required = terminals.len().saturating_sub(2);
        Ok(plans
            .iter()
            .filter_map(|path| {
                let local: Vec<usize> = path
                    .iter()
                    .map(|&i| positions[self.sites[i].rf.id.as_str()])
                    .collect();
                crate::topology::path_installation(&graph, &local, self.settings)
            })
            .map(|mut solution| {
                solution.diagnostics.push(format!(
                    "Exact widest-path search over {screened} screened links; {rounds} certification rounds at final resolution."
                ));
                if required > 0 {
                    solution.diagnostics.push(format!(
                        "The route passes through all {required} required router{}.",
                        if required == 1 { "" } else { "s" }
                    ));
                }
                solution
            })
            .collect())
    }

    /// Publish certified routes while refinement continues, so a stopped search
    /// can keep them.
    fn publish_snapshot(
        &mut self,
        mut certified: Vec<NetworkSolution>,
    ) -> Result<(), OptimizationError> {
        if certified.is_empty() || self.hooks.cancelled() {
            return Ok(());
        }
        for solution in &mut certified {
            solution
                .diagnostics
                .push("Certified route snapshot; router positions are still being refined.".into());
        }
        self.restore_profiles(&mut certified)?;
        if self.hooks.cancelled() {
            return Ok(());
        }
        let result = self.result(certified, false);
        self.snapshot = Some(result.clone());
        if let Some(publish) = self.hooks.solution_progress.as_deref_mut() {
            publish(result);
        }
        Ok(())
    }

    pub(super) fn run_path_search(
        mut self,
        terminals: &BTreeSet<usize>,
    ) -> Result<OptimizationResult, OptimizationError> {
        self.exhaustive_screen(terminals, true)?;
        if self.hooks.cancelled() {
            return Ok(self.interrupted());
        }
        self.hooks.notify(Stage::SearchingTopologies, 0, 1);
        let (mut plans, rounds) = self.certified_plans()?;
        self.screened_links = self.coarse.len() + self.links.len();
        self.coarse = Vec::new();
        if self.hooks.cancelled() {
            return Ok(self.interrupted());
        }
        let certified = self.installations(&plans, terminals, rounds)?;
        self.publish_snapshot(certified)?;
        if self.hooks.cancelled() {
            return Ok(self.interrupted());
        }
        let moved = self.refine_routes(&mut plans)?;
        if self.hooks.cancelled() {
            return Ok(self.interrupted());
        }
        if moved > 0 {
            self.diagnostics.push(format!(
                "Moved {moved} router position{} within the {:.0} m refinement radius to strengthen their hops.",
                if moved == 1 { "" } else { "s" },
                self.settings.refine_radius_m
            ));
        }
        let mut alternatives = self.installations(&plans, terminals, rounds)?;
        if self.hooks.cancelled() {
            return Ok(self.interrupted());
        }
        self.hooks.notify(Stage::SearchingTopologies, 1, 1);
        if alternatives.is_empty() {
            self.diagnose_bridge_failure(terminals)?;
        }
        self.restore_profiles(&mut alternatives)?;
        if self.hooks.cancelled() {
            return Ok(self.interrupted());
        }
        Ok(self.finish(alternatives))
    }
}

impl Search<'_, '_> {
    /// Offer the exact routes as seeds to the general topology search used by
    /// the other priorities.
    pub(super) fn seed_planned_routes(&mut self) {
        if self.sites.iter().filter(|site| site.is_client()).count() != 2 {
            return;
        }
        let plans = self.plan_on(&self.link_graph(&self.links));
        for path in plans {
            self.seeds.push(
                path[1..path.len() - 1]
                    .iter()
                    .map(|&i| self.sites[i].rf.id.clone())
                    .collect(),
            );
        }
    }
}

# Long-route search tests on the Kartverket raster

The [132-run sweep](kartverket-route-sweep-2026-11-20.csv) compares modeled routes on six fixed corridors. Coordinates are in EPSG:25832. All runs used the [50 m Kartverket DTM fixture](../fixtures/kartverket-nhm-25832-50m.md), bounds inset 50 m on each edge, and terrain-sampled endpoint elevations. The earlier experiment with endpoint ground elevations left at zero is excluded.

The baseline was commit `8f84b56`. The updated planner included the local changes to optimizer graph, topology and search. Both used release builds, eight workers, two 3 m endpoint antennas, automatic candidates, Maximum Reliability, a request for two independent paths, default RF settings including a 10 dB fade reserve, and a 50 km maximum link distance. Times cover one optimization call each, excluding raster loading and compilation.

Both planners ran five search configurations on each corridor. A temporary prototype that introduces pairs of internally disjoint three-hop chains as topology search seeds ran the same 30 configurations. Its outputs are labeled paired_seed_prototype. The updated planner also ran three router-limit and validation combinations and three candidate-grid spacing probes on each corridor, plus an endpoint-reversal check at 32 neighbors and the default candidate cap and router limit. For the spacing probes, setting the refinement radius to zero let the requested spacing take effect. With the default 250 m refinement radius, requested grid spacings of 750 m and 1,000 m both generate a 250 m grid. Candidate cap excludes the two clients. A zero alternative count means the search returned no route under that configuration.

Primary margin is the minimum usable margin across the primary path, taking the weaker RF direction on each hop. Network margin is the minimum across all selected links. Primary line-of-sight and Fresnel obstruction counts are separate. All reported primary hops had valid final RF results with profiles. These are bare-earth predictions for generated sites.

The [alternative networks](kartverket-route-alternatives-2026-11-20.csv) came from six additional runs of the updated planner, one per corridor, with propagation validation, 32 neighbors, an 800-candidate cap, 250 m effective grid and up to ten routers. The selected column identifies the active alternative. Networks with the same number of independent paths can differ in installation count, primary clearance, and weakest-link margin.

## Integrated search

The [integrated search measurements](kartverket-route-integrated-2026-11-20.csv) repeat all 30 baseline propagation configurations and add 24 strict-LOS configurations. Compared with commit `8f84b56`, primary bottleneck margin increased in 26 propagation runs and tied in four. All 30 preserved or increased achieved path count. Median elapsed time across these configurations was 0.51 s, versus 0.24 s for the baseline. Each configuration was timed once.

On the 95 km full corridor, strict validation found a seven-hop route at 125 m spacing with six routers and 20.38 dB of usable margin on its weakest hop. At 250 m spacing, the ten-router limit and 32 neighbors yielded a ten-hop route at 19.40 dB. The six-router, 16-neighbor configuration at that spacing returned no route. Every hop of each returned strict primary path met both clearance checks.

To rerun the strict-route regression, set `REPEATERNET_TERRAIN_KARTVERKET_FIXTURE` to the raster in the fixture manifest. From the `rust` directory run `cargo test --release --locked -p repeaternet-optimizer --test kartverket_routes -- --ignored`.

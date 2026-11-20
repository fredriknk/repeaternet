
# RepeaterNet RF core

`repeaternet-rf-core` calculates terrain-aware, directional radio link budgets.
It shares RF types and math with the route optimizer and coverage engine.
Coordinates and elevations are in metres, frequency is in MHz, absolute radio
power is in dBm, and gain and loss are in dB or dBi as named.

## Integration

- Use `Point`, `Site`, `SiteKind` and `HeightReference` for projected sites.
  Supply ground elevation and antenna height above ground or above the
  modelled surface. A missing surface elevation falls back to ground.
- Implement `TerrainSource` to provide a positive ground resolution and
  ordered samples for each `TerrainLayer`. Ground samples must be finite.
  Missing surface samples fall back to ground. A wrong sample count or
  missing ground produces an `RfError`.
- Use `LinkEvaluator::new` with terrain and `RfSettings`, then `evaluate`
  to calculate both directions over one path. `EvaluationOptions` allows
  per-site `RadioBudget` overrides, a requested sampling step and an optional
  retained `TerrainProfile`. `LinkResult` includes the weaker margin and
  validity. `optimistic_margin_db` uses free-space loss and peak antenna
  gains for route screening.
- Use `sample_profile` for terrain, effective Earth curvature, line of
  sight and first Fresnel zone clearance. Fresnel clearance measures how much
  of the radio path's first Fresnel zone is free of obstruction. The k-factor
  scales Earth's effective radius to approximate refraction. The link
  evaluator adds Bullington diffraction and can apply surface clutter loss.

## Defaults and limits

The library defaults are 869.5 MHz, 22.0 dBm transmit power, −130.0 dBm
receiver sensitivity, 10.0 dB fade margin, a k-factor of 1.0 and 60% required
Fresnel clearance. In propagation mode both directional margins must pass.
Strict line-of-sight mode also requires clear line of sight and the configured
Fresnel clearance. Optional LoRa sensitivity uses bandwidth, noise figure and
the spreading-factor SNR threshold when the manual override is off.
[RF settings](src/settings.rs)

Sampling includes at least three points and both endpoints. The count uses
the greater of the requested step and the terrain resolution, then distributes
samples evenly along the path. Callers enforce their sample budgets before
evaluation. `ElevationPattern::from_csv` reads a UTF-8 CSV with `elevation_deg` and
`gain_dbi` headers. It sorts samples, interpolates between them and uses the
nearest endpoint gain outside their range. Use `AntennaPattern` for constant
gain. The library receives typed settings and
the application owns HTTP and saved-plan formats.
[Profile sampling](src/profile.rs) · [Antenna patterns](src/antennas.rs)

## Tests

From `rust/`, run `cargo test --locked -p repeaternet-rf-core`.
Unit tests use synthetic terrain and reference values for path loss, Fresnel
clearance, curvature and diffraction. They check Python-derived flat, ridge
and valley profiles, the antenna CSV parser, missing terrain, LoRa sensitivity,
directional budgets and scalar/full-profile agreement. [Test cases](src/tests.rs)

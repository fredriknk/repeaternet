# User guide

## Projects and saved data

The project selector manages named projects in the current browser workspace.
Plan edits are autosaved. Rename, duplicate, archive, and restore projects from
the project controls. Duplication copies the plan and terrain; it does not clone
a live calculation. Archived projects remain on disk and are available through
**Archived projects**. **Recover previous** restores the previous saved plan
revision.

Permanent deletion removes the selected project's stored data. Terrain clearing
removes the chosen terrain copies. Archiving retains storage. The workspace
storage indicator helps distinguish these operations.

A route's compact summary survives a server restart; rerun the saved plan to
regenerate full route profiles. Completed coverage runs are stored separately
and can be loaded or reused. Export a `.webplan.json` for a portable copy of
settings and sites. It does not contain terrain.

Browser cookies identify the workspace. A different browser or cleared cookie
does not automatically reopen the same workspace. There are no user accounts or
cross-browser project sharing controls.

## Terrain and coordinates

Set endpoints with map placement, marker dragging, or latitude/longitude fields.
The map uses WGS84 coordinates; GeoJSON uses longitude/latitude ordering.
RF calculations use the terrain's projected CRS.

Upload tiled GeoTIFFs sharing one CRS:

- **DTM:** bare-earth ground elevation, used for antenna bases and site quality.
- **DOM:** surface elevation, including available tree/building heights, used
  for obstruction profiles.

A ground-mounted antenna is DTM plus height above ground. Missing DOM produces
an explicit terrain-only assumption. Missing ground samples are unknown and
cannot certify a link or coverage cell.

Kartverket preparation estimates tile count, effective resolution, pixel budget,
and cache reuse before download. Large areas are divided into corridor tiles.
Automatic resolution may coarsen the requested download to fit its pixel budget.
Inspect that estimate before accepting it. Downloaded tiles are validated before
becoming active. Uploaded terrain and RF computation work locally; online terrain
retrieval and basemaps require network access.

Service definitions have one source:
[packaged Kartverket configuration](../src/rf_router_planner/data/kartverket_wcs.json).
The provider chooses a configured Norwegian ETRS89 UTM service from the planning
coordinates.

## Route planning

Load nearby repeaters from CoreScope when existing infrastructure should be
considered. Filter the catalog and assign Optional, Required, or Excluded policy.
Required sites must participate; optional sites compete with other candidates.
The infrastructure policy controls whether the search uses existing sites,
proposed sites, or both.

Select a search objective and effort. The engine generates terrain candidates,
screens possible RF links, searches for routes, and validates selected links at
the configured final sampling step. Results include feasible alternatives for
exact router counts. Reliability is the achieved number of node-independent
paths, rather than how many nearby repeaters exist.

Ready-terrain searches start with one click. Missing terrain prompts for one
combined download-and-search action. The job card shows progress and cancellation.

**Map** and **Analysis** are separate views. Analysis presents a compact
alternative selector, route summary, and link/profile selector. Expand the
comparison or directional budgets when needed. Help disclosures keep explanations
out of the main form. Selecting an alternative reuses evaluated results; it does
not launch a new route search.

Edits to endpoints, radio, terrain, or sites mark affected results stale.
A proposed site can be pinned by moving it, but the edited network needs a new
search. Undo/Redo applies to plan edits. Route CSV and GeoJSON exports describe
the selected validated result.

## Predicted mesh coverage

Coverage uses the selected route's repeaters, optionally a subset or added
endpoints, and a separate client radio profile. It evaluates downlink and uplink
independently. Choose the mesh buffer, map extent, or a drawn polygon as the area.

Calculate and Quick preview first estimate the work, then ask for one
confirmation. Preview is approximate and limited to 256 cells and 4,096 source
evaluations. Standard calculations default to a 4,096-cell budget, with settings
bounded to 16,384 cells, 64 sources, and 250,000 cell/source evaluations.
The estimate reports effective grid spacing and coarsening.

Results stream onto the map. Views show two-way, downlink, uplink, overlap, or
best-serving source. Inspect a location for exact-point directional margins,
network membership, and a terrain/Fresnel profile. Samples outside a drawn area,
missing terrain, and work that could not be completed remain explicit states.

Identical completed runs can reuse verified saved results when terrain, route,
sources, grid, radio, and model metadata match. Changed inputs require
recalculation. Saved grid output is limited to 100 MiB; exceeding the limit
reports a failure rather than silently evicting another run.

## Scenarios, targets, and reports

Height scenarios calculate coverage with chosen source heights. Compare saved
runs sharing compatible terrain, grid, radio, and client assumptions. Differences
show gains, losses, and unresolved cells. Applying scenario heights changes the
plan and requires route revalidation.

Router-failure analysis distinguishes local coverage from coverage connected
to a selected reference router. A locally reachable source may be isolated
from the surviving mesh.

Save point, road, and area targets or import target GeoJSON. Target assessment
uses exact sampling for points/roads and conservative saved-grid evaluation for
areas. Unresolved or unknown samples do not become passing targets. Compatible
saved target reports can be compared.

Coverage exports include JSON, GeoJSON cell/target geometry, and a printable
HTML report with an offline SVG map. Reports can include target assessments and
a compatible scenario comparison. Stale or partial historical runs require
explicit opt-in. Inspect assumptions, sample spacing, unknown states, and run
identity when sharing a report.

## Interpreting RF results

The model combines free-space loss, antenna gains/feed losses, effective-Earth
curvature, Fresnel clearance, Bullington terrain diffraction, optional configured
clutter loss, and receiver sensitivity. Forward and reverse budgets are separate;
an RF edge must pass in both directions.

```text
FSPL = 32.44 + 20 log10(distance_km) + 20 log10(frequency_MHz)
received power = TX power + TX gain - TX feed loss
                 - FSPL - diffraction - clutter
                 + RX gain - RX feed loss - miscellaneous loss
usable margin = received power - receiver sensitivity - required fade margin
```

In propagation mode an obstructed link can still pass the configured RF budget.
Strict LOS mode additionally requires clear LOS and the configured Fresnel
clearance. “Passes configured validation criteria” therefore must be read
alongside obstruction, clearance, and sampling information.

Diffraction uses the Bullington terrain component, not the full
delta-Bullington/spherical-Earth method. The default effective-Earth factor is
4/3 and the Fresnel target is 60%. These assumptions are configurable.

The web workflow supports two endpoints. Longer-route search is bounded and
heuristic; it does not prove global optimality across all terrain. Final sampled
profiles can miss narrow obstacles between samples. Coverage is a sampled
prediction, not measured radio emissions or a packet-delivery guarantee.
The infrastructure objective is an installation/mast-height proxy, not a
monetary quote. Field calibration, ownership/access constraints, and a full
terrain/climate propagation model remain future work.

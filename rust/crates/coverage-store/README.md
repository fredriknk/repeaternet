# RepeaterNet coverage store

Project-scoped persistence for bounded coverage runs, result pages, and target
reports. The crate requires Rust 1.89 or newer for standard-library file
locking. Builds and tests need no Python; the checked-in goldens are frozen
oracles from the removed Python implementation.

## Server integration

1. Resolve the authorized project directory and ID through the project catalog.
   Call `Store::open(directory, project_id)`. Use a writable data directory
   controlled by the service. Call `recover_interrupted` on first use after reopening the project, before starting a worker. An active
   worker would have its persisted run interrupted by this call.
2. Derive `CurrentIdentity::new` from the live certified alternative,
   `snapshot_version`, project ID, and current route, terrain, settings, and
   radio. Return no current identity when the certified route is missing or
   its input fingerprint differs from the active plan. Use `route_fingerprint`
   for the route plan and `fingerprint` for already normalized, encoded
   coverage and radio settings. Keep terrain provenance fresh and deterministic,
   including the route and coverage terrain layers in their effective order.
   Optionally include `model_version` (for example `coverage-rust-v1`) to bind
   currency to one native model. `new` rejects `PYTHON_MODEL_VERSION`.
3. Build the expected run manifest from the approved request, including the
   fields in `REUSE_FIELDS`, the encoded `settings` and `radio_settings`
   objects, `coverage_store_format: FORMAT`, and `stale: false`. Call
   `find_reusable(&expected, &current)` before scheduling RF work. On a miss,
   assign a new lowercase 32-hex job ID and call `start` with state `queued`
   or `running`. The store assigns the result path, format, and watermark.
   `start` rejects a missing or empty `model_version` and the Python
   `coverage-v3`, so native and Python RF output never share an identity.
   Identity fields may be filled in by progress once the grid is known. Once
   set, these fields and the embedded settings/radio snapshot are immutable.
4. Call `progress(id, patch, cells)` for counters and row-major JSON cell
   chunks. Pass an empty cell slice for metadata-only progress. The worker
   supplies counters, stages, and state transitions. Use `finish` for
   `complete`, `cancelled`, `failed`, `stale`, or `interrupted`. A storage
   failure leaves the last committed manifest visible. If a quota failure
   should terminate the worker, call `finish` again with state `failed`, the
   failure stage, and an empty slice.
5. Use `current`, `load(id)`, and `history` for manifests. Python manifests
   returned by these methods always have `stale: true` and a `stale_reason`.
   The `*_with_legacy` variants refine that reason with a `LegacyIdentity`.
   Native manifests are returned as stored. Derive their status at response
   time with `job_stale_reason(&job, current, legacy)`, or with
   `CurrentIdentity::stale_reason`/`matches` (`route_stale_reason`/
   `route_matches` for route-only inspection). `None` means current.
   `page(id, cursor, limit, current)` and `page_with_identities` do this
   automatically. They return the Python page fields plus `stale_reason`.
   The cursor counts stored rows, and limits range from 1 to 256.
   `visit_cells(id, visitor)` streams stored rows for comparison and export.
   Both verify JSONL framing, object state, and index ordering. Paging scans
   to EOF for the exact stored count and corruption detection.
6. Pass a report with `project_id`, `coverage_job_id`, `report_id`, the
   matching `terrain_fingerprint`, `radio_fingerprint`, and `model_version`,
   and a `targets` array to `save_report`. The store verifies that the run is
   current and complete, limits storage, and publishes immutable reports.
   Python runs are never current, so they cannot receive new reports. Python
   behaves the same way after a restart. `load_report` looks up by run and
   report ID, `find_report` by report ID across the project, and `reports`
   returns the latest 100 summaries. Generate target fingerprints and
   comparison metadata in the caller.
7. For comparisons, call `Store::comparison(baseline, scenario,
   current_terrain)`, where `current_terrain` is the live coverage-mosaic
   fingerprint (`CurrentIdentity::terrain_fingerprint` or
   `LegacyIdentity::terrain`), or call `legacy_comparison` with a
   `LegacyIdentity`. Both need the same project and complete state, one
   fingerprint domain (both native or both Python), one model version, the
   live terrain in both runs, one radio fingerprint, and intact embedded
   settings/radio hashes. Both also need a valid grid and a fully stored,
   ordered result stream. A failure returns `Error::Invalid(reason)` with a
   `reason::COMPARISON_*`/`reason::DOMAIN`/`reason::INCOMPLETE` constant. The
   caller still checks the Python settings keys (`profile_step_m`,
   `maximum_profile_samples`, `client`), grid keys (`rows`, `columns`,
   `effective_cell_size_m`, `area_bounds_wgs84`), and the reference router.
   `comparison_mismatch` exposes the identity part without disk access.
8. Wrap disk-backed export generation with `write_export` at a trusted
   server-selected path. It streams through a 100 MiB writer and publishes the
   file atomically. The HTTP layer owns response streaming. The current server uses the export
   crate's temporary spool for download responses.

Use blocking workers for store calls in an async server. A visitor runs under
the project lock and must not call back into the store. The HTTP layer owns
worker scheduling, cancellation signals, live route checks, page footprint
projection, partial/stale export opt-ins, wire error mapping, comparison,
and report computation. The coverage engine can stay independent of the
store by serializing its cells to JSON values at the integration boundary.

### Stale reasons

`reason::*` constants are stable `&'static str` values. Native runs report the
first failing identity, in this order: `unsupported_legacy_model_or_format`,
`persisted_stale`, `model_version_changed`, `project_changed`,
`route_changed`, `alternative_changed`, `snapshot_changed`,
`terrain_changed`, `settings_changed`, `radio_changed`,
`embedded_settings_invalid`. Without a live identity, the reason is
`current_identity_unavailable`. Python runs report
`legacy_identity_unavailable` without a `LegacyIdentity`. Otherwise they report
the first of `persisted_stale`, `legacy_project_changed`,
`legacy_terrain_changed`, `legacy_embedded_settings_invalid`, and finally
`legacy_python_route_not_current`. Older Python model versions report
`unsupported_legacy_model_or_format`.

## Disk and identity guarantees

- Preserves Python artifact names and field shapes. The current manifest is
  `coverage-job.json`. Completed or interrupted current runs are moved into
  `coverage-jobs/<id>.json` on rollover. The current manifest takes precedence
  over a same-ID archive. Results use `coverage-results/<id>.jsonl` and reports
  use `coverage-target-reports/<job-id>-<report-id>.json`.
- The `.coverage-store.lock` project file lock serializes cooperating native readers and writers.
  Result data is synced before atomic manifest replacement. The manifest
  watermark makes a torn or otherwise uncommitted result tail invisible to
  native readers, and the next append or startup recovery truncates it.
  Publication covers process interruption. Power-loss durability across
  filesystems and simultaneous Python writers are outside this protocol.
  A crash during initial creation may leave an unreferenced empty result file.
  Generate a new job ID when retrying an interrupted start.
- Results and exports are capped at 100 MiB. The store reads one bounded JSONL
  chunk at a time. Reports are capped at 5 MiB of stored UTF-8, including a
  final newline, and 20 per run. Manifests have a separate 5 MiB read and
  write cap. Archive reuse considers the current run and up to 50 archived
  manifests in descending modification order.
- Lookup IDs must be lowercase 32-hex and the project ID, path filename, and
  embedded IDs must agree. Result paths must exactly match their run ID.
  Artifact directories and files reject symbolic links and Windows reparse
  points, including junctions. The project root's ancestors and export
  destination are trusted service configuration. The project lock coordinates
  store processes and does not protect against local actors with write access
  to the data directory.
- The native fingerprint domain uses SHA-256 over the format tag, a zero byte,
  and compact JSON with sorted object keys. Number representations are
  preserved by the JSON value, so callers should use consistent serialization
  for normalized inputs. Native reuse verifies the entire run identity,
  embedded settings and radio hashes, and every indexed row of a complete
  grid. An on-disk row in the `not_evaluated` state invalidates reuse. Rows
  with evaluated outcomes such as `unknown_terrain` and `outside_area` may
  be retained and reused.

## Python compatibility

### Proven against Python goldens

`tests/fixtures/python-goldens.json` was produced by
`tests/fixtures/generate_python_goldens.py` (removed; see [baseline commit `18b7cee`](https://github.com/fredriknk/repeaternet/tree/18b7cee/rust/crates/coverage-store/tests/fixtures)), which called the functions in
`rf_router_planner.web` at baseline commit `18b7cee` with CPython 3.13.12 on
Windows 11. `tests/identity.rs` checks:

- `python_canonical_json`/`python_fingerprint` reproduce `_plan_fingerprint`
  byte-for-byte on 3,288 cases. These cover about 3,200 floats
  (random bit patterns, scaled values, 17- and 25-digit inputs, and
  round-half-even decimals), subnormals, `±0.0`, the `1e16` and `1e-5`
  notation thresholds, uppercase exponents, arbitrary-size and `-0`
  integers, control characters, DEL, `ensure_ascii` surrogate pairs,
  non-NFC text, code point key ordering, duplicate keys, nesting, and
  whitespace. Non-finite inputs (`1e400`) are refused rather than hashed as
  Python's `Infinity`.
- `python_route_fingerprint` matches `_route_plan_fingerprint`, including
  unknown outer plan fields. `python_reuse_fingerprint` matches
  `_coverage_reuse_fingerprint`. The default `encode(parse_coverage_settings
  ({}))` and radio `{rf, candidates}` hashes match, and the goldens include
  their exact Python JSON for normalizer comparison.
- `python_terrain_entry` and `python_terrain_fingerprint` reproduce
  `_terrain_fingerprint` for both the route and `coverage=True` sets. The
  checks cover nested generation paths, non-ASCII names, unsorted input,
  zero-size files, and pre-1970 and nanosecond `st_mtime_ns` values. The
  same test passes on Windows NTFS and in a Linux container.
- Python manifests with `NaN`/`Infinity` (written with `allow_nan`, for
  example in `report_links` margins) are readable, with those values mapped
  to `null`. Files with the native format marker stay strict.

### Why a Python run is never current or reusable

Python's `coverage_current` requires the in-memory `ws.result`,
`ws.result_plan_fingerprint` and `ws.snapshot_version`. These reset on every
process start (`snapshot_version = 0`), so after a restart Python itself treats
every saved run as stale until a new route result exists. A later match would
then rest only on a coincidental counter value. No native process can prove
that a Python run belongs to its live route. Its RF output is also a different
model (`coverage-v3` against the native `coverage-rust-v1`). The store
therefore:

- never reuses Python output (`find_reusable` and `reuse_fingerprint` are
  native-only) and never lets a native run claim `coverage-v3`;
- reports Python runs as stale with the most specific provable reason,
  ending with `legacy_python_route_not_current` when terrain, project and
  embedded hashes all verify;
- supports Python-to-Python comparison, which Python allows after a restart
  because it depends only on persistent terrain/radio/settings identity. It
  rejects mixed native/Python comparisons with `DOMAIN`.

Python checks only `settings_fingerprint` for currency. The native check also
requires `radio_fingerprint` and the embedded hashes, which is stricter.

### Residual blockers and caller obligations

- **Normalization parity is the caller's job.** These hashes only match Python
  when the input JSON has Python's types and values. That means `1.0` and `1`
  stay distinct, number tokens are kept, and the encoded settings equal
  `encode(parse_coverage_settings(...))`. `serde_json` must use
  `arbitrary_precision`, and the plan must keep its stored values rather than
  be re-serialized through typed structs. The goldens include the default
  Python settings/radio JSON for comparison. A native normalizer has not been
  proven equal for non-default inputs.
- **Terrain file discovery belongs to the server.** Python globs
  `<kind>/*.tif`, `terrain-generations/generation-*/<kind>/*.tif` and, for
  coverage, `coverage-terrain-generations/generation-*/<kind>/*.tif` for
  `dtm` and `dom`. Its glob is case-insensitive on Windows, matches dotfiles,
  and follows symlinks. A stricter discovery, such as one that rejects links
  or `.TIF`, can only make a run look changed. It never makes a run look
  current. `python_terrain_entry` follows links like `Path.stat()`.
- NTFS stores 100 ns timestamps. Python and Rust both read them as multiples
  of 100 ns, so goldens use 100 ns-aligned values. Times that a FAT/exFAT copy
  rounds will differ, as they would for Python.
- Python strings with lone surrogates (accepted by `json.loads`) cannot be
  represented in Rust. Such manifests fail with `Error::Json`.
- An unreadable current `coverage-job.json` (corrupt JSON or wrong project)
  returns an error from every operation that reads it. Python ignores a bad
  manifest on restore. The dashboard state handler can omit unavailable saved coverage status.
  Endpoints that explicitly read the corrupt manifest can still fail.
- The goldens were generated on Windows only. Linux evidence covers
  re-running the Rust side, not generating new goldens with Python on Linux.

## Verification

From `rust/`, run `cargo test --locked -p repeaternet-coverage-store` and
`cargo clippy --locked -p repeaternet-coverage-store --all-targets -- -D warnings`. The integration tests include Python-format data, Python
goldens, project and path isolation, a real Windows junction or Unix symlink,
interrupted and competing writes, reuse identity and corrupt/truncated
results, legacy/native stale reasons and comparisons, and storage quotas.
The goldens are frozen. Regenerating them requires checking out `18b7cee`, where the Python reference and generator still exist.

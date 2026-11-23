# Release validation

The Windows workflow runs formatting, Clippy, workspace tests, dashboard tests and a second terrain and coverage test pass with resident windows disabled. A `v*` tag also builds and verifies the Windows x64 executable, packages it and publishes the ZIP with a `SHA256SUMS.txt` for the ZIP. Tags containing a hyphen are published as prereleases. The clean-machine check is manual. [Windows workflow](../.github/workflows/ci.yml)

## Windows executable

Use PowerShell on Windows and run the examples from the repository root. [build-rust.ps1](../tools/build-rust.ps1) takes no parameters and builds `rust/target/release/repeaternet.exe` using the workspace toolchain. You can also build from `rust/` with Cargo and pass the resulting path to both scripts. CI builds with the explicit `x86_64-pc-windows-msvc` target and uses that target's release directory.

```powershell
$exe = 'rust/target/release/repeaternet.exe'
```

## Verify a candidate

```powershell
$reportJson = & .\tools\verify-rust-release.ps1 -ExePath $exe -RequirePeTool
if ($LASTEXITCODE -ne 0) { throw ($reportJson -join "`n") }
$report = $reportJson | ConvertFrom-Json
$report | ConvertTo-Json -Depth 8
```

| Verifier parameter | Default and effect |
| --- | --- |
| `-ExePath` | `rust/target/release/repeaternet.exe`. Selects the candidate to copy and run. |
| `-TerrainFixture` | The checked-in `fallback.tif`. Use the same bytes at another path or a compatible fixture for the fixed route in the script. |
| `-TimeoutSeconds` | `30`. Accepts 5 through 120 seconds for bounded process, startup and route waits. Each HTTP request uses the smaller of 10 seconds and this value. |
| `-RequirePeTool` | Off. Fails when no supported direct import inspector is available. CI leaves the switch off. |

The script copies the executable by itself into a unique scratch working directory. It uses explicit scratch data and cache paths, a Windows-only child `PATH`, a generated login token and a loopback listener. Use a temp directory whose path is free of brackets and backticks because Windows PowerShell 5.1 treats those characters specially in process log redirection. The verifier sends a synthetic EPSG:25833 GeoTIFF over HTTP. The isolated child runs without Python, a source checkout, asset files or provider access. [Default fixture](../rust/crates/terrain/tests/fixtures/fallback.tif)

The JSON report records the copied binary's version and SHA-256 and the result of each completed check. The first failure stops further checks. Cleanup stops the processes the verifier launched and removes its scratch directory after a containment check. Exit code `1` indicates verification or cleanup failure. Cleanup errors report the scratch path.

- Verify health, sign-in, incorrect token rejection, foreign-origin write rejection, the login redirect and the two session cookies. Fetch the registered CSS and JavaScript paths listed in the script and all three Leaflet marker PNGs. [Verifier asset list](../tools/verify-rust-release.ps1)
- Create and autosave a project, upload the terrain, calculate a route with a sampled profile and export GeoJSON.
- Confirm that a second process fails to bind the same port while the original stays healthy. Force-stop and restart on the same address, then check the saved plan, uploaded terrain and persisted route summary with the original cookies. Sign in with the same token in a fresh cookie session and verify that it sees the first session's project in the shared catalog. Create another project in the second session and check that both sessions keep their own active project and see the new project in the catalog.
- Inspect direct PE imports with the first available `objdump`, `dumpbin` or `llvm-readobj`. Unexpected imports fail the check. With `-RequirePeTool`, a missing tool also fails. Otherwise the report marks PE inspection as skipped. The import table covers imports visible to the tool. Dynamic loads and Windows version compatibility still require runtime checks.

## Package the verified executable

```powershell
New-Item -ItemType Directory -Path dist -Force | Out-Null
& .\tools\package-rust-release.ps1 -ExePath $exe -ExpectedSha256 $report.sha256
```

| Packager parameter | Default and effect |
| --- | --- |
| `-ExePath` | `rust/target/release/repeaternet.exe`. Selects the Windows x64 candidate. |
| `-OutputPath` | `dist/repeaternet-windows-x64.zip`. The parent must exist and the ZIP name must be unused. |
| `-CargoManifestPath` | `rust/Cargo.toml`. Selects the workspace and lockfile for the dependency inventory. |
| `-NoticeManifestPath` | `rust/notices/release-notices.json`. Selects the reviewed notice manifest with its adjacent `texts/` directory. |
| `-ExpectedSha256` | Unset. A 64-digit hexadecimal verifier hash checks the executable bytes. |
| `-AllowIncompleteNotices` | Off. Allows an audit ZIP for gaps recorded in the notice manifest. |

The packager validates the Windows x64 PE header and checks the executable
hash when `-ExpectedSha256` is supplied. It includes a separate `SHA256SUMS.txt`
inside the ZIP for `repeaternet.exe`. It publishes the ZIP after verifying the
entries and reports both the executable and ZIP hashes. Fixed timestamps and
entry order make the ZIP reproducible with identical inputs and the same
packaging environment. [Packaging script](../tools/package-rust-release.ps1)

Use the build's workspace, lockfile and Rust toolchain for the third-party inventory. `-CargoManifestPath` selects the build workspace manifest. The packager runs Cargo metadata and Cargo tree with `--locked --offline` and uses the cached crate sources, the matching `rust-docs` copyright file and the [offline notice manifest](../rust/notices/release-notices.json). The manifest applies supplements by crate name. Review the selected licenses, version-specific notices and upstream sources when dependencies change. It lists upstream license texts for crates that ship none, and the license selected for dual-licensed crates. If a new crate has no license file, packaging fails until its text is added under `rust/notices/texts/` with a supplement entry. `-NoticeManifestPath` can select another reviewed manifest with its `texts/` directory beside it. Record the workspace and toolchain when building the executable. The binary hash alone cannot identify its dependency graph.

The inventory reflects Cargo's default-feature tree for the `repeaternet` package targeting `x86_64-pc-windows-msvc`. It covers normal and build dependencies and excludes dev dependencies. Normal paths that do not cross a procedural macro are labeled `target`. The rest are labeled `build/proc-macro`. Target reachability does not establish that a crate contributed linked code.

The packager preserves crate-relative paths for discovered license files, including nested and declared `license-file` paths. Supplemental texts are checked against their recorded SHA-256 hashes; a crate's own file takes precedence over a supplement with the same name. It reopens the temporary ZIP and verifies the executable and notice entry hashes before publishing. A missing or changed supplemental file fails even with `-AllowIncompleteNotices`. The switch permits an audit ZIP for upstream gaps recorded in the manifest and reports `packaged_with_notice_gaps`. Review those gaps and applicable upstream terms before distribution.

The ZIP includes Leaflet's versioned license, bundled SQLite and Rust standard library notices, and the attribution and terms link for the catalog derived from the IOGP EPSG Geodetic Parameter Dataset v12.029 via PyProj 3.8.0 and PROJ 9.8.1. The archive's `licenses/release-notices.json` records upstream sources, excerpt ranges and hashes. [Terrain catalog provenance](../rust/crates/terrain/README.md) · [IOGP EPSG Dataset Terms of Use](https://iogp.georepository.com/terms-of-use.html) · [Leaflet 1.9.4 license](https://github.com/Leaflet/Leaflet/blob/v1.9.4/LICENSE) · [SQLite copyright](https://sqlite.org/copyright.html)

## Clean-Windows Explorer and browser test

The automated run uses a fresh directory on the development PC. Complete this manual test on a clean Windows VM or PC for each supported Windows version, using a fresh standard-user profile and no development tools, Python, GIS runtime or application-specific DLLs installed for RepeaterNet. Record the Windows version, browser and executable SHA-256 with the results.

1. Verify the downloaded ZIP against the published `SHA256SUMS.txt`, then extract it and verify `repeaternet.exe` against the checksum file inside the ZIP. Place just the executable in a new folder, preferably with spaces in its path, and double-click it in Explorer. Check that the default browser opens after startup at `http://127.0.0.1:8000/` and the dashboard loads. Include the launch behavior of the actual downloaded artifact.
2. In the browser, create a project and set A to `59.994961, 15.000896` and B to `59.994961, 15.005198`. In **Terrain**, upload the synthetic fixture as ground terrain. Choose **Existing only** for the infrastructure policy and select **Find repeater route**. Check the map. Open **Analysis** to check the terrain profile and downloaded GeoJSON. Stop and relaunch the server at the same address with the same browser profile. Confirm that the project, terrain, route and profile return.
3. From a terminal, launch with `--no-browser` and a separate scratch `--data` path. Check that no browser window opens automatically and that Ctrl+C shuts down the listener. With a listener already on the requested port, check the terminal error and that the running dashboard is still available. Test token sign-in with `RF_PLANNER_TOKEN` set in that terminal and a fresh browser session.

## Historical development-machine record

On 2026-11-19 two `repeaternet 0.1.0` executables passed the smoke workflow under Windows PowerShell 5.1. The last verified executable had SHA-256 `270128636F7170D04E86D10617D2356A64A849A6D914094452CBB55A82B24F36`. Its route returned 13 profile samples and the contending process exited with code `1`. `objdump` reported `advapi32.dll`, `api-ms-win-core-synch-l1-2-0.dll`, `bcrypt.dll`, `bcryptprimitives.dll`, `crypt32.dll`, `kernel32.dll`, `ntdll.dll` and `ws2_32.dll` after deduplication. A missing-fixture run exited with code `1` and cleaned its scratch directory.

The first executable, SHA-256 `5F267725AF38E17B11FF8E5E71953D88664AF1980B0059CDB20FB3E625B2D909`, predated the available source and lockfile. Two packaging runs for that binary produced identical ZIP hashes. Packaging the last verified executable also preserved its exact bytes. Its smoke result covers that artifact, and integration source files changed afterward. The available Cargo inventory identified 222 dependencies and reported 10 with no discovered license text. Rerun the workflow with the final candidate and its recorded build inputs. The clean-Windows Explorer and browser test remains pending.

## Historical notice audit for a Windows x64 executable

The notice audit reused the verified `repeaternet.exe` with SHA-256 `936AD9E94232F399FC9FF50B6A8887AEF7E064279E552C3078DF0AAF7377B385`. The output is `dist/repeaternet-windows-x64-936ad9e94232-notice-audit.zip`, SHA-256 `C7F7D1BA6EF5D1CF9AAEE0A2CF3257D2DC439C8E9F1E5D45014C2955760B8708`. The executable entry matches both the on-disk executable and the entry in `dist/repeaternet-windows-x64-936ad9e94232.zip` byte for byte. The inventory uses Cargo.lock SHA-256 `78F06C277CC87E4814A13D575B719C02CC8DCEAE6ABA57EDEEB86B3FE90243EE` and Rust 1.100.0 (`0ef1bc386`).

```powershell
& .\tools\package-rust-release.ps1 `
  -ExpectedSha256 936AD9E94232F399FC9FF50B6A8887AEF7E064279E552C3078DF0AAF7377B385 `
  -OutputPath dist/repeaternet-windows-x64-936ad9e94232-notice-audit.zip `
  -AllowIncompleteNotices
```

The prior ZIP used a 222-crate metadata inventory. The package-specific Windows tree contains 213 external crates, with 190 on normal target paths and 23 reachable only through build or procedural macro paths. The nine metadata-only entries are `cfg_aliases`, `chacha20`, `cpufeatures`, `lru-slab`, `quinn`, `quinn-proto`, `quinn-udp`, `rustc-hash` and `tinyvec`. Cargo tree excludes them for this target and feature selection. The ZIP includes the nested notices for Ring's Fiat and once_cell code and tracing-core's spin code. The selected SQLite feature set uses bundled SQLite, so the cached SQLCipher tree is outside this inventory.

Windows PowerShell 5.1 produced a byte-identical ZIP on a repeat run. The negative runs published no ZIP when the executable hash was wrong, the shared LERC MIT input was missing or corrupted, a required nested Ring path was absent, or the audit switch was omitted. The archive passed an independent ZIP integrity check and byte comparisons for all manifest-listed notice files. All 19 supplemental source files and excerpts matched their referenced upstream bytes.

- The repository root license texts cover `geo 0.33.1`, `i_key_sort 0.10.3`, `i_overlay 4.5.2`, `lerc-core 0.5.2`, `lerc-reader 0.5.2`, `rstar 0.12.2`, `tiff-core 0.11.1` and `tiff-reader 0.11.1`. Each source revision matches the crate's published `.cargo_vcs_info.json`. The LERC repository is now under `roteiro-gis`. [Source revisions and exact file hashes](../rust/notices/release-notices.json)
- For `proj4rs 0.1.10`, the pinned source declares MIT or Apache 2.0. The ZIP includes the Apache Software Foundation's Apache 2.0 text and the four original PROJ copyright and permission headers in the published Rust source. A crate-wide MIT text or project copyright notice remains unresolved. [Pinned proj4rs source](https://github.com/3liz/proj4rs/tree/bc266a822ee74215814d5478b5de85939c2cd94b) · [Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0.txt)
- For `unicode-casefold 0.2.0`, the ZIP includes the canonical Apache 2.0 text, the Unicode 9.0.0 data copyright header and the contemporaneous Unicode permission notice published in ICU 58.2. The versioned Unicode input regenerated the crate's tables byte for byte. The upstream revision matches the published crate's Rust sources, generator and README byte for byte and lists version `0.1.0` in its manifest. A crate-wide MIT text or project copyright notice for the published `0.2.0` crate remains unresolved. [Published manifest](https://docs.rs/crate/unicode-casefold/0.2.0/source/Cargo.toml) · [Matching upstream files](https://github.com/lambda-fairy/unicode-casefold/tree/a390d60eb53311169d9780ad70bd68985c44e7f3) · [Unicode 9.0.0 CaseFolding data](https://www.unicode.org/Public/9.0.0/ucd/CaseFolding.txt) · [ICU 58.2 license](https://github.com/unicode-org/icu/blob/release-58-2/icu4c/LICENSE)


## License-choice correction

The two audit gaps above concerned an unselected MIT alternative. The historical package selected Apache-2.0 for proj4rs 0.1.10 and unicode-casefold 0.2.0 under their published Cargo license declarations. The current lockfile uses proj4rs 0.3.1 and adds proj4rs-geodesic. The current notice manifest records Apache-2.0 selections for proj4rs, proj4rs-geodesic and unicode-casefold. [Current lockfile](../rust/Cargo.lock) · [Current notice manifest](../rust/notices/release-notices.json) Cargo uses OR for a license choice and documents slash as the deprecated syntax. The published unicode-casefold manifest declares `MIT/Apache-2.0`. [Cargo license fields](https://doc.rust-lang.org/cargo/reference/manifest.html#the-license-and-license-file-fields)

Packaging verifies the declared expressions for the selected licenses and the presence of the hash-validated Apache text. The manifest also supplies applicable PROJ, GeographicLib and Unicode notices.

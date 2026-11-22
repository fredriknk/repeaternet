# RF calibration reference engines

Run this stage from the repository root with the Miniconda base Python 3.13
and its installed NumPy. The reference stage does no network I/O at runtime.

```powershell
python -I tools/rf-calibration/reference_engines.py
python -I -m unittest discover tools/rf-calibration/tests
```

## External dependencies

Use the existing model-review directory via `--review-dir DIR` or
`RF_REVIEW_DIR`. The final fallback is
`C:\Users\fnk\pythonprojects\repeaternet\rust\target\rf-review`. To prepare a
new review directory, create it outside the repository and put the following
dependencies there.

1. Check out NTIA ITM v1.3 at `652b3fa698a3c5026dd5d68ac06a84c254e29dcc`
   under `vendor/ntia-itm`. With the MSYS2 UCRT64 `g++` on `PATH`, run the
   following commands from the new review directory. [NTIA ITM v1.3](https://github.com/NTIA/itm/tree/v1.3)

   ```powershell
   git clone https://github.com/NTIA/itm.git vendor/ntia-itm
   git -C vendor/ntia-itm checkout 652b3fa698a3c5026dd5d68ac06a84c254e29dcc
   $itmSources = (Get-ChildItem vendor/ntia-itm/src/*.cpp).FullName
   g++ -shared -O2 -static -static-libgcc -static-libstdc++ -o ntia-itm.dll @itmSources
   ```

2. Check out Py1812 under `vendor/py1812` at the pinned revision below. The
   runner loads its source directly. [Py1812 pinned source](https://github.com/eeveetza/Py1812/tree/6c9061dd022e93c84169059a016643926ce89964)

   ```powershell
   git clone https://github.com/eeveetza/Py1812.git vendor/py1812
   git -C vendor/py1812 checkout 6c9061dd022e93c84169059a016643926ce89964
   ```

3. Download the **Zip (Components)** from ITU-R P.1812-7 and extract it so
   that `DN50.TXT` and `N050.TXT` are under
   `vendor/itu-recommendations/maps7/`. The runner loads the grids with
   `numpy.loadtxt` and writes `P1812.npz` under `<work>/cache/p1812-<hash>/`.
   The hash uses the two map files' contents. The archive is reused on future
   runs and the review directory stays read-only. [ITU-R P.1812-7 components and scatter notice](https://www.itu.int/rec/R-REC-P.1812-7-202308-S)

## Inputs and outputs

`--work-dir DIR` overrides `RF_CALIBRATION_DIR` and the default
`%LOCALAPPDATA%\RepeaterNet\calibration`. The script reads `profiles.jsonl`
and `dataset/links.jsonl` from that directory. `--links PATH` overrides the
latter. It joins on `ProfileRecord.link_ids` and reverses the sample order for
links starting at endpoint `b`. It reports the number of links without
profiles. Paths resolving into the live `%LOCALAPPDATA%\RepeaterNet\data`
directory are rejected.

Each engine writes `<work>/predictions/<engine>.jsonl` in ascending link ID,
surface, Tx height and Rx height order. Each file is replaced atomically when
complete. Rows from the previous file are reused when the raw profile hash,
direction, frequency, height grid and engine fingerprint still match. The
fingerprint includes the runner, reference source or DLL, map inputs, Python
and NumPy versions. Map archives persist across runs.

| Engine | Rows per surface and height pair |
| --- | --- |
| `itm_p2p` | 50% confidence, 50% reliability, `mdvar=0` |
| `itm_p2p_t90` | 50% confidence, 90% time reliability, `mdvar=1` |
| `p1812` | Advised P.1812-6 scatter with mapped terrain, followed by the terminal woodland scenario |
| `py1812_delta_bullington` | Vertical `dl_delta_bull` with `k=1`, land electrical properties throughout and `flag4=0` |

`--engines itm_p2p,p1812,py1812_delta_bullington` is the default selection.
Selecting `itm_p2p` also writes `itm_p2p_t90`. All engines use DTM ground for
antenna heights, including on DOM, and use the stored frequency or 869.5 MHz
when missing. `fspl_db` uses 3-D antenna separation and the 32.45 dB constant.
`excess_db` is the joint model total minus that FSPL. P.1812 woodland changes
propagation through the profile itself. Its increment over the mapped case is
in `diag.woodland_increment_db`, with the joint loss in `excess_db` and
`clutter_db` set to zero. The P.1812 diagnostics also include the optional
published -7 scatter calculation, using the named horizon and scatter angles
from Py1812. The earlier review read the path-type value in place of the
scatter angle, so its published -7 comparison values differ.

The default height grid is 2, 5, 10 and 20 m at each end. P.1812 runs across
processes by link, with the default `--jobs` equal to CPU count minus one
(minimum one). The three-link smoke run took about 0.46 s of P.1812 wall time
with three active workers, so the default retains all 16 height pairs.
`--p1812-heights 2,5,10` runs only those nine P.1812 height pairs. Each P.1812
row records `diag.height_grid_m` and `diag.height_grid_subset`. When an input
has fewer samples than Py1812 requires, the runner subdivides each segment
equally and preserves all original samples. `step_m` records the evaluated
spacing and `diag.input_step_m` records the input spacing.

## Private smoke run

The tests always run the standalone synthetic checks. Model checks require
their review dependencies. The three-link oracle also requires the private
review's `drammenmesh_links.json`, `drammenmesh_sites.json` and `results.json`.
Keep those files and the converted profiles outside the repository.

```powershell
$rfSmoke = Join-Path $env:LOCALAPPDATA 'RepeaterNet/calibration/scratch/d3'
python -I tools/rf-calibration/tests/convert_review.py --work-dir $rfSmoke
python -I tools/rf-calibration/reference_engines.py --work-dir $rfSmoke
```

Each engine prints elapsed wall time and summed worker time per computed
link-height pair and per prediction. A pair covers both surfaces when
available and both P.1812 scenarios. On repeat runs with unchanged inputs,
the summary reports reused links and skips model evaluation for them.

# User-guide screenshots

Run this tool manually before opening a PR for a major dashboard change.
It walks the Oslo–Harestua route and coverage workflows in installed Google
Chrome and captures the PNGs referenced by the [user guide](../../docs/USER_GUIDE.md).

## Setup and run

Install Node.js 22 or later with npm, Google Chrome, and the
[repo's Rust build prerequisites](../../docs/DEVELOPMENT.md#verification).
From the repository root:

```sh
cd rust
cargo build --release -p repeaternet
cd ../tools/docs-screenshots
npm ci
npm run capture
```

The tool launches the release binary on a free loopback port and uses a
fresh, temporary data directory. A healthy full run takes roughly 30–60
seconds. Slow providers or retries can add several minutes. Route data
comes from Kartverket and CoreScope, and map views use OpenStreetMap
tiles. The Search image shows the Overpass road filter enabled. The live
route search uses the guide's normal settings with that filter disabled.

## Options

Pass options after `npm run capture --`. Relative paths are resolved
from your current directory.

| Option | Effect |
| --- | --- |
| `--exe <path>` | Use a different binary. Defaults to `rust/target/release/repeaternet.exe` on Windows and `rust/target/release/repeaternet` on Linux. |
| `--only <name[,name]>` | Capture selected images and run their prerequisites. For example, `npm run capture -- --only map-legend`. Names may include `.png`. |
| `--out <dir>` | Write images to a different directory. Defaults to `docs/images`. |
| `--keep-data` | Retain the temporary data directory for debugging. The server still stops. |

The summary lists skipped images and exits nonzero if any requested image
could not be captured. Existing PNGs for those images stay in place.
**Look at every changed image before committing** and update the guide
as needed, per [UX rules §9](../../docs/UX_DESIGN_RULES.md#9-definition-of-done-for-a-ui-change).

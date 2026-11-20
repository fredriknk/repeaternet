"""Regenerate the embedded EPSG catalog and its PyProj reference transforms."""

import csv
import math
import warnings
from pathlib import Path

from pyproj import CRS, Transformer
from pyproj.database import query_crs_info
from pyproj.exceptions import CRSError

HERE = Path(__file__).resolve().parent
METHODS = {"utm", "tmerc", "lcc", "laea", "aea", "merc", "stere", "sterea", "eqc", "cea"}
BASES = {4326, 4258, 4171, 4167}
UTM = [*range(32601, 32661), *range(32701, 32761), *range(25828, 25838)]


def candidates():
    for info in query_crs_info(auth_name="EPSG", pj_types="PROJECTED_CRS"):
        code = int(info.code)
        if code >= 32767 or code in UTM:
            continue
        crs = CRS.from_epsg(code)
        base = crs.geodetic_crs.to_epsg()
        if base not in BASES:
            continue
        if any(axis.unit_conversion_factor != 1 for axis in crs.axis_info):
            continue
        try:
            with warnings.catch_warnings():
                warnings.simplefilter("ignore", UserWarning)
                definition = crs.to_proj4().removesuffix(" +no_defs +type=crs")
        except CRSError:
            continue
        if definition.split()[0].removeprefix("+proj=") not in METHODS:
            continue
        if definition.startswith("+proj=eqc ") and code != 4087:
            # The Rust ellipsoidal EQC adapter has an explicit EPSG:4087 definition.
            continue
        if any(key in definition for key in ("+axis=", "+towgs84=", "+pm=")):
            continue
        if code == 3857:
            # These use spherical projection equations with the WGS84 semi-major axis.
            definition = "+proj=webmerc +ellps=WGS84 +units=m"
        elif "+nadgrids=" in definition:
            continue
        yield code, (base, definition.replace("+proj=tmerc ", "+proj=etmerc "))


def samples(code):
    crs = CRS.from_epsg(code)
    west, south, east, north = crs.area_of_use.bounds
    if east < west:
        east += 360
    transform = Transformer.from_crs(4326, crs, always_xy=True)
    for fx, fy in ((0.37, 0.37), (0.63, 0.63), (0.24, 0.72), (0.76, 0.28)):
        lon = (west + (east - west) * fx + 180) % 360 - 180
        lat = south + (north - south) * fy
        x, y = transform.transform(lon, lat)
        assert math.isfinite(x) and math.isfinite(y), code
        yield code, lon, lat, x, y


definitions = dict(sorted(candidates()))
(HERE.parent.parent / "src/epsg.txt").write_text(
    "".join(f"{code} {base} {definition}\n" for code, (base, definition) in definitions.items()),
    encoding="ascii", newline="\n",
)
with (HERE / "epsg_transforms.csv").open("w", encoding="ascii", newline="") as handle:
    writer = csv.writer(handle, lineterminator="\n")
    writer.writerow(("epsg", "lon", "lat", "x", "y"))
    for code in sorted([*definitions, *UTM]):
        writer.writerows(samples(code))

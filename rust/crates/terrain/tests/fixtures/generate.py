"""Regenerate synthetic fixtures with the repository's Python environment."""

import csv
import struct
from pathlib import Path

import numpy as np
import rasterio
from affine import Affine
from pyproj import CRS, Transformer
from rasterio.transform import from_origin
from shapely.geometry import LineString, MultiPoint

from rf_router_planner.models.site import Site
from rf_router_planner.terrain.raster import ArrayTerrain, RasterTerrain
from rf_router_planner.terrain.sampling import sample_profile

HERE = Path(__file__).resolve().parent
BASE = from_origin(500000, 6651000, 10, 10)
ROTATED = BASE * Affine.rotation(21) * Affine.shear(9)


def write(name, values, transform=BASE, crs="EPSG:25833", mask=None, point=False, **options):
    with rasterio.Env(GDAL_TIFF_INTERNAL_MASK=True):
        with rasterio.open(
            HERE / name, "w", driver="GTiff", width=values.shape[1],
            height=values.shape[0], count=1, dtype=values.dtype,
            crs=crs, transform=transform, **options,
        ) as dataset:
            dataset.write(values, 1)
            if mask is not None:
                dataset.write_mask(mask)
            if point:
                dataset.update_tags(AREA_OR_POINT="Point")


def table(name, header, rows):
    with (HERE / name).open("w", encoding="utf-8", newline="") as handle:
        writer = csv.writer(handle, lineterminator="\n")
        writer.writerow(header)
        writer.writerows(rows)


y, x = np.indices((260, 270))
values = (100 + x * 0.25 + y * 0.5).astype("float32")
values[102, 42] = -9999
values[257, 258] = -9999
write(
    "rotated.tif", values, ROTATED, nodata=-9999, compress="deflate",
    predictor=3, tiled=True, blockxsize=64, blockysize=64,
)

mask_values = (np.arange(18 * 18).reshape(18, 18) + 100).astype("int16")
mask_values[0, 0] = -9999  # Explicitly valid even though it matches the nodata value.
mask = np.full((18, 18), 255, dtype="uint8")
mask[0, 1] = 0
mask[17, 17] = 0
write(
    "masked.tif", mask_values, nodata=-9999, mask=mask, compress="lzw",
    predictor=2, tiled=True, blockxsize=16, blockysize=16, BIGTIFF="YES",
)
write(
    "fallback.tif", np.full((18, 18), 500, dtype="uint16"),
    from_origin(500000, 6651000, 20, 20),
)
write(
    "point.tif", np.array([[1.25, np.nan], [3.25, 4.25]], dtype="float64"),
    crs="EPSG:25832", nodata=np.nan, point=True, compress="zstd", ENDIANNESS="BIG",
)
write(
    "nonfinite.tif", np.array([[np.nan, np.inf], [-np.inf, 4]], dtype="float32"),
    compress="packbits",
)
write(
    "float_nodata.tif", np.array([[0.1, 1.0]], dtype="float32"), nodata=0.1,
)
write(
    "unprojected.tif", np.array([[1]], dtype="uint8"),
    from_origin(10, 61, 0.1, 0.1), crs="EPSG:4326",
)
write("unsupported_crs.tif", np.array([[1]], dtype="uint8"), crs="EPSG:27700")
write("feet.tif", np.array([[1]], dtype="uint8"), crs="EPSG:2263")
write(
    "custom_datum_unsupported.tif", np.array([[1]], dtype="uint8"),
    crs="+proj=tmerc +lat_0=2 +lon_0=-99.25 +k=0.998 +x_0=400000 "
        "+y_0=5000 +datum=NAD83 +units=m",
)
write("no_crs.tif", np.array([[1]], dtype="uint8"), crs=None)
sparse = np.full((32, 32), -9999, dtype="int16")
sparse[:16, :16] = 17
write(
    "sparse.tif", sparse, nodata=-9999, compress="deflate",
    tiled=True, blockxsize=16, blockysize=16, SPARSE_OK="TRUE",
)

raster_cases = {
    "rotated.tif": [
        ROTATED * p for p in [
            (269.1, 259.8), (0.1, 0.9), (42.5, 102.5), (64.2, 63.8),
            (63.8, 64.2), (258.8, 257.8), (125.5, 259.5), (268.9, 200.4),
            (-0.01, 30), (269, -0.01), (280.0, 120.0), (269.1, 259.8),
        ]
    ],
    "masked.tif": [
        BASE * p for p in [(0.5, 0.5), (1.5, 0.5), (17.5, 17.5), (16.25, 16.75), (0, 0), (18, 2), (2, 18)]
    ],
    "point.tif": [BASE * p for p in [(0, 0), (1.5, 0.5), (0.5, 1.5), (2, 0), (0, 2)]],
    "sparse.tif": [BASE * p for p in [(0, 0), (16.5, 0.5), (31.5, 31.5)]],
    "float_nodata.tif": [BASE * p for p in [(0.5, 0.5), (1.5, 0.5)]],
}
samples, metadata = [], []
for name, coordinates in raster_cases.items():
    with RasterTerrain([HERE / name]) as terrain:
        xs, ys = map(np.asarray, zip(*coordinates, strict=True))
        samples.extend((name, x, y, z) for x, y, z in zip(xs, ys, terrain.sample(xs, ys), strict=True))
    with rasterio.open(HERE / name) as dataset:
        metadata.append((name, min(dataset.res), *dataset.bounds, *dataset.transform.to_gdal()))
table("raster_samples.csv", ["file", "x", "y", "z"], samples)
table(
    "raster_metadata.csv",
    ["file", "resolution", "min_x", "min_y", "max_x", "max_y", "x0", "a", "b", "y0", "d", "e"],
    metadata,
)

coordinates = [
    (25832, 5.3221, 60.39299), (25832, 10.7522, 59.9139),
    (25833, 10.7522, 59.9139), (25833, 18.9553, 69.6492),
    (25833, 23.9, 70.8), (25833, 8.5, 58.1),
    (25835, 29.725, 70.074), (25835, 35.9, 78.22),
]
table(
    "coordinates.csv", ["epsg", "longitude", "latitude", "x", "y"],
    [(epsg, lon, lat, *Transformer.from_crs(4326, epsg, always_xy=True).transform(lon, lat))
     for epsg, lon, lat in coordinates],
)

ground = np.full((1, 7), 100.0)
surface = np.array([[100, 125, 130, np.nan, 135, 110, 100]])
terrain = ArrayTerrain(ground, resolution_m=100, dom=surface)
profile = sample_profile(
    terrain, Site("A", 0, 0, ground_elevation_m=100, antenna_height_m=15),
    Site("B", 600, 0, ground_elevation_m=100, antenna_height_m=25),
    869.5, sample_step_m=100,
)
fields = [
    "distances_m", "x", "y", "dtm_elevation_m", "surface_elevation_m",
    "earth_bulge_m", "effective_obstruction_m", "los_elevation_m",
    "fresnel_radius_m", "clearance_m", "fresnel_clearance_ratio",
]
table("profile.csv", fields, zip(*(getattr(profile, key) for key in fields), strict=True))

extra_points = [(7050, 1500), (10000, 2000)]
bends = {
    "square": [(0, 0), (7000, 0)],
    "diagonal": [(0, 0), (7000, 7000)],
    "bent": [(0, 0), (7000, 0), (7000, 1700)],
    "acute": [(0, 0), (7000, 0), (500, 550)],
    "loop": [(0, 0), (5000, 0), (5000, 5000), (0, 5000), (0, 0), (5000, 0)],
}
corridors = []
for name, vertices in bends.items():
    geometry = LineString(vertices).buffer(200, cap_style="square", join_style="mitre")
    if name == "bent":
        geometry = geometry.union(MultiPoint(extra_points).buffer(200))
    corridors.append((name, *geometry.bounds, geometry.area))
table("corridors.csv", ["name", "min_x", "min_y", "max_x", "max_y", "area"], corridors)

# Place the reference lon/lat inside a pixel so CRS and TIFF decoding are tested together.
etrs_tm = CRS.from_proj4(
    "+proj=tmerc +lat_0=3 +lon_0=13.25 +k=0.998 +x_0=400000 +y_0=5000 +ellps=GRS80 +units=m"
).to_json_dict()
etrs_base = CRS.from_epsg(4258).to_json_dict()
del etrs_base["$schema"], etrs_base["type"]
etrs_tm["base_crs"] = etrs_base
projected_cases = [
    ("utm_wgs84_north", 32633, 14.9, 60.1, {}),
    ("utm_wgs84_south", 32755, 147.1, -42.8, {"BIGTIFF": "YES"}),
    ("utm_etrs89", 25834, 21.2, 69.1, {"GEOTIFF_VERSION": "1.1"}),
    ("web_mercator", 3857, 14.9, 60.1, {}),
    ("europe_laea", 3035, 14.9, 60.1, {}),
    ("world_equidistant_cylindrical", 4087, -73.4, 41.8, {}),
    ("world_cylindrical_equal_area", 6933, 22.1, -43.2, {}),
    ("custom_tm", "+proj=tmerc +lat_0=2 +lon_0=13.25 +k=0.998 +x_0=400000 "
                  "+y_0=5000 +datum=WGS84 +units=m", 14.9, 60.1, {}),
    ("custom_etrs_tm", CRS.from_json_dict(etrs_tm), 14.9, 60.1, {"GEOTIFF_VERSION": "1.1"}),
    ("custom_lcc", "+proj=lcc +lat_0=50 +lon_0=15 +lat_1=45 +lat_2=60 +x_0=300000 "
                   "+y_0=800000 +datum=WGS84 +units=m", 14.9, 60.1, {}),
    ("custom_aea", "+proj=aea +lat_0=40 +lon_0=15 +lat_1=43 +lat_2=65 +x_0=300000 "
                   "+y_0=800000 +datum=WGS84 +units=m", 14.9, 60.1, {}),
    ("custom_laea", "+proj=laea +lat_0=53 +lon_0=13 +x_0=400000 +y_0=5000 "
                    "+datum=WGS84 +units=m", 14.9, 60.1, {}),
    ("custom_laea_south", "+proj=laea +lat_0=-90 +lon_0=13 +x_0=400000 +y_0=5000 "
                          "+datum=WGS84 +units=m", 14.9, -68.2, {}),
    ("custom_lcc_scale", "+proj=lcc +lat_0=48 +lat_1=48 +lon_0=13 +k=0.997 +x_0=400000 "
                         "+y_0=5000 +datum=WGS84 +units=m", 14.9, 60.1, {}),
    ("custom_merc_scale", "+proj=merc +lon_0=13 +k=0.997 +x_0=400000 +y_0=5000 "
                          "+datum=WGS84 +units=m", 14.9, 60.1, {}),
    ("custom_merc_parallel", "+proj=merc +lon_0=13 +lat_ts=27 +x_0=400000 +y_0=5000 "
                             "+datum=WGS84 +units=m", 14.9, 60.1, {}),
    ("custom_polar_scale", "+proj=stere +lat_0=90 +lon_0=13 +k=0.997 +x_0=400000 "
                           "+y_0=5000 +datum=WGS84 +units=m", 14.9, 70.1, {}),
    ("custom_polar_parallel", "+proj=stere +lat_0=-90 +lon_0=13 +lat_ts=-71 +x_0=400000 "
                              "+y_0=5000 +datum=WGS84 +units=m", 14.9, -68.2, {}),
    ("custom_oblique_stereo", "+proj=sterea +lat_0=52 +lon_0=13 +k=0.997 +x_0=400000 "
                              "+y_0=5000 +datum=WGS84 +units=m", 14.9, 60.1, {}),
    ("custom_eqc", "+proj=eqc +lon_0=13 +lat_ts=20 +x_0=400000 +y_0=5000 "
                   "+datum=WGS84 +units=m", 14.9, 60.1, {}),
    ("custom_cea", "+proj=cea +lon_0=13 +lat_ts=20 +x_0=400000 +y_0=5000 "
                   "+datum=WGS84 +units=m", 14.9, 60.1, {}),
]
projected_rows = []
for name, crs, lon, lat, options in projected_cases:
    name += ".tif"
    x, y = Transformer.from_crs(4326, crs, always_xy=True).transform(lon, lat)
    write(name, np.array([[42]], dtype="int16"), from_origin(x - 5, y + 5, 10, 10),
          CRS.from_user_input(crs).to_wkt(), **options)
    with rasterio.open(HERE / name) as dataset:
        assert dataset.crs.is_projected and dataset.crs.linear_units == "metre"
        rx, ry = Transformer.from_crs(4326, dataset.crs, always_xy=True).transform(lon, lat)
        assert abs(x - rx) < 1e-6 and abs(y - ry) < 1e-6
        (sample,) = next(dataset.sample([(x, y)]))
    projected_rows.append((name, CRS.from_user_input(crs).to_epsg() or "", lon, lat, x, y, sample))
table("projected_rasters.csv", ["file", "epsg", "lon", "lat", "x", "y", "z"], projected_rows)


def replace_geokey(source, name, key, replacement):
    # Mutate one entry of a small Rasterio-authored little-endian classic TIFF.
    # A conforming writer will not produce these conflicting key combinations.
    data = bytearray((HERE / source).read_bytes())
    assert data[:4] == b"II*\x00"
    ifd, = struct.unpack_from("<I", data, 4)
    entry_count, = struct.unpack_from("<H", data, ifd)
    for tag, dtype, count, offset in struct.iter_unpack(
        "<HHII", data[ifd + 2: ifd + 2 + entry_count * 12],
    ):
        if tag != 34735:
            continue
        assert dtype == 3 and count >= 4
        _, _, _, key_count = struct.unpack_from("<4H", data, offset)
        for index in range(key_count):
            entry = offset + 8 + index * 8
            if struct.unpack_from("<H", data, entry)[0] == key:
                struct.pack_into("<4H", data, entry, *replacement)
                (HERE / name).write_bytes(data)
                return
    raise ValueError(f"Missing GeoKey {key} in {source}")


replace_geokey("custom_tm.tif", "conflicting_projection.tif", 3072, (3072, 0, 1, 32633))
replace_geokey("custom_tm.tif", "conflicting_angular_units.tif", 2054, (2054, 0, 1, 9105))
replace_geokey("custom_tm.tif", "unsupported_datum_shift.tif", 2061, (2062, 34736, 3, 0))
replace_geokey("custom_tm.tif", "duplicate_geokey.tif", 3092, (1025, 0, 1, 1))
replace_geokey("custom_tm.tif", "missing_projection_parameter.tif", 3092, (4099, 0, 1, 9001))
replace_geokey("custom_tm.tif", "invalid_double_offset.tif", 3092, (3092, 34736, 1, 65535))
replace_geokey("web_mercator.tif", "conflicting_base.tif", 2054, (2048, 0, 1, 4258))

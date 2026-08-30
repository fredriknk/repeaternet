"""Generate a small GeoTIFF with two ridges for GUI demonstrations."""
from pathlib import Path

import numpy as np


def main() -> None:
    try:
        import rasterio
        from rasterio.transform import from_origin
    except ImportError as exc:
        raise SystemExit("Install rasterio to generate the example") from exc
    width, height, resolution = 1201, 401, 10.0
    x = np.arange(width)
    ridge1 = 350 * np.exp(-((x - 400) / 35) ** 2)
    ridge2 = 450 * np.exp(-((x - 800) / 45) ** 2)
    dtm = np.tile((ridge1 + ridge2).astype("float32"), (height, 1))
    destination = Path(__file__).with_name("synthetic_two_ridges.tif")
    with rasterio.open(destination, "w", driver="GTiff", width=width, height=height, count=1,
                       dtype="float32", crs="EPSG:25833",
                       transform=from_origin(250_000, 6_700_000, resolution, resolution), nodata=-9999) as dataset:
        dataset.write(dtm, 1)
    print(destination)


if __name__ == "__main__":
    main()


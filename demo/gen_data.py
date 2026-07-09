#!/usr/bin/env python3
"""Generate a small synthetic demo dataset for the DuckDB Martin tile server.

Theme: a Singapore rainfall / flood sensor network (ties into a flood-dashboard
story). Produces a few dozen point features scattered across Singapore with
plausible rainfall / water-level readings and a status flag.

Outputs (written next to this script):
  * sg_flood_sensors.geojson   -- always (stdlib only; consumed via ST_Read)
  * sg_flood_sensors.parquet   -- only if `pyarrow` is installed (GeoParquet-ish)

The GeoJSON is the default the server config points at, so this script has zero
hard dependencies. Parquet is optional and demonstrates the analytics path.
"""

from __future__ import annotations

import json
import math
import os
import random

HERE = os.path.dirname(os.path.abspath(__file__))

# Singapore bounding box (lon/lat) we scatter sensors within.
LON_MIN, LON_MAX = 103.62, 104.04
LAT_MIN, LAT_MAX = 1.23, 1.46

# A few real-ish place anchors so the points look like a sensible network.
ANCHORS = [
    ("Marina Barrage", 103.871, 1.281),
    ("Bishan", 103.848, 1.351),
    ("Jurong East", 103.742, 1.333),
    ("Tampines", 103.945, 1.353),
    ("Woodlands", 103.786, 1.437),
    ("Changi", 103.989, 1.357),
    ("Bukit Timah", 103.776, 1.354),
    ("Sentosa", 103.830, 1.249),
]

STATUSES = ["normal", "normal", "normal", "elevated", "flood-warning"]

N_FEATURES = 48
SEED = 1337


def jitter(value: float, span: float, rng: random.Random) -> float:
    return value + rng.uniform(-span, span)


def clamp(v: float, lo: float, hi: float) -> float:
    return max(lo, min(hi, v))


def build_features() -> list[dict]:
    rng = random.Random(SEED)
    features: list[dict] = []
    for i in range(N_FEATURES):
        anchor = ANCHORS[i % len(ANCHORS)]
        name = f"{anchor[0]} Sensor {i // len(ANCHORS) + 1}"
        lon = clamp(jitter(anchor[1], 0.03, rng), LON_MIN, LON_MAX)
        lat = clamp(jitter(anchor[2], 0.025, rng), LAT_MIN, LAT_MAX)

        rainfall = round(abs(rng.gauss(12.0, 9.0)), 1)          # mm/hr
        # Higher rainfall -> higher water level, with noise.
        water = round(clamp(0.2 + rainfall * 0.04 + rng.gauss(0, 0.1), 0.0, 5.0), 2)
        if water >= 1.6:
            status = "flood-warning"
        elif water >= 1.0:
            status = "elevated"
        else:
            status = rng.choice(STATUSES)

        features.append(
            {
                "type": "Feature",
                "geometry": {"type": "Point", "coordinates": [round(lon, 6), round(lat, 6)]},
                "properties": {
                    "station_id": f"SG-{1000 + i}",
                    "name": name,
                    "rainfall_mm": rainfall,
                    "water_level_m": water,
                    "status": status,
                },
            }
        )
    return features


def write_geojson(features: list[dict]) -> str:
    path = os.path.join(HERE, "sg_flood_sensors.geojson")
    fc = {"type": "FeatureCollection", "features": features}
    with open(path, "w", encoding="utf-8") as fh:
        json.dump(fc, fh)
        fh.write("\n")
    return path


def write_parquet(features: list[dict]) -> str | None:
    """Best-effort GeoParquet output via pyarrow. Stores geometry as WKB so that
    DuckDB `read_parquet` + `ST_GeomFromWKB` / spatial can consume it.

    If pyarrow is unavailable, returns None (the GeoJSON path is the default).
    """
    try:
        import pyarrow as pa
        import pyarrow.parquet as pq
    except Exception:
        return None

    def wkb_point(lon: float, lat: float) -> bytes:
        # Little-endian WKB Point (type 1).
        import struct

        return struct.pack("<BIdd", 1, 1, lon, lat)

    station_id = [f["properties"]["station_id"] for f in features]
    name = [f["properties"]["name"] for f in features]
    rainfall = [f["properties"]["rainfall_mm"] for f in features]
    water = [f["properties"]["water_level_m"] for f in features]
    status = [f["properties"]["status"] for f in features]
    geom = [wkb_point(*f["geometry"]["coordinates"]) for f in features]

    table = pa.table(
        {
            "station_id": pa.array(station_id, pa.string()),
            "name": pa.array(name, pa.string()),
            "rainfall_mm": pa.array(rainfall, pa.float64()),
            "water_level_m": pa.array(water, pa.float64()),
            "status": pa.array(status, pa.string()),
            "geom": pa.array(geom, pa.binary()),
        }
    )
    path = os.path.join(HERE, "sg_flood_sensors.parquet")
    pq.write_table(table, path)
    return path


def main() -> None:
    features = build_features()
    gj = write_geojson(features)
    print(f"wrote {len(features)} features -> {gj}")

    pqp = write_parquet(features)
    if pqp:
        print(f"wrote GeoParquet            -> {pqp}")
    else:
        print("pyarrow not installed; skipped Parquet (GeoJSON is the default source)")

    # Quick sanity summary.
    warn = sum(1 for f in features if f["properties"]["status"] == "flood-warning")
    print(f"flood-warning sensors: {warn}/{len(features)}")


if __name__ == "__main__":
    main()

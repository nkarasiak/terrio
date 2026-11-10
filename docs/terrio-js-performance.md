# terrio-js against the JavaScript libraries it would replace

Work of 2026-11-09 and 2026-11-10 on the fork, tracked in nkarasiak/terrio#22. The goal: per-format
terrio-js modules that read cloud formats as fast as geotiff.js, zarrita and hyparquet, at a size
close to theirs. This file sums up what was implemented, measured, tried and dropped.

## Method

- Node.js 22, single-threaded, from Europe. Medians; a fresh process (or browser context) per run;
  checksums equal across libraries unless noted.
- "main" is terrio-js from `main` (95fd254); "branch" adds the fixes below.
- terrio builds are `wasm-fast` (wasm-pack `--profile wasm-fast --no-opt`, no LTO) unless a size
  is given; both sides of an A/B use the same profile and features.
- The reference library is measured once on local files; on remote files (S3) it runs interleaved
  with terrio, as network drift reaches ±30%.

## Branches

Each is one commit on `main` (95fd254) unless noted, pushed to the fork, no pull request yet.

| Branch | Formats | Change |
|---|---|---|
| `perf/zarr-blosc-bitshuffle` | Zarr, native and JS | Blosc bit unshuffle by 8 × 8 bit transposes instead of a bit at a time |
| `perf/zarr-js-open-round` | Zarr (JS) | the array's `zarr.json` asked for in the first round at open: one round trip fewer |
| `perf/js-request-window` | every remote read (JS) | 12 requests in flight, the next starting as soon as one ends; a failed Zarr request cancels the other bodies |
| `perf/zarr-js-shard-split` | Zarr, Node.js/Deno/Bun | shard runs split into the 12 requests in flight instead of runs of up to 1 MiB |
| `perf/js-open-growth` | COG, PMTiles, COPC, FlatGeobuf, GeoParquet (JS) | each request at open at least as long as the bytes read so far: a 655 KB COG header in 4 requests instead of 8 |
| `perf/js-range-split` (on `perf/js-open-growth`) | same formats, Node.js/Deno/Bun | runs of adjacent tiles as parallel pieces of at least 512 KiB |
| `perf/geoparquet-wkb` | GeoParquet, native and JS | geometries decoded with the GeoPackage driver's WKB reader (moved to `drivers/vector/wkb.rs`), on the column's bytes without a copy |
| `perf/js-vector-read-no-copy` | GeoParquet, FlatGeobuf, PMTiles (JS) | `RemoteDataset.read` without a bbox takes the layers instead of copying them |
| `perf/js-geojson-export` | every vector layer (JS) | `toGeoJSON`: geometries built in JS from wasm memory; the rest written as JSON and parsed 8 MB at a time. Strings with control characters are now escaped; non-finite numbers become `null` |
| `perf/zarr-fill-doubling` | Zarr, native and JS | a region's fill value repeated by doubling instead of one 8-byte copy per element (36 ms of a 40 MB read with a NaN fill) |
| `perf/zarr-reorder-rows` | Zarr, native and JS | a window converted row by row when columns are the last dimension, instead of element by element |

Merge branches, for benchmarks:

- `perf/zarr-js-remote`: the first six.
- `perf/geoparquet-js-bench`: `perf/zarr-js-remote` and the three GeoParquet branches.
- `perf/zarr-js-local-bench`: `perf/geoparquet-js-bench` and the two Zarr read branches; this file.
- `spike/zarr-core-size`: the size spike (CRS stubbed out, not for merge), with its crate in
  `spikes/zarr-size/`.

## COG: terrio-js against geotiff.js 3.0.5

Cloud COGs, open and first read:

| Case | main | branch | geotiff.js |
|---|---|---|---|
| Sentinel-2 B04, 2048² window (us-west-2) | 4.80 s | **2.58 s** | 3.98 s |
| Sentinel-2 B04, overview (us-west-2) | 3.68 s | **2.65 s** | 4.12 s |
| Sentinel-2 TCI RGB, 2048² window (us-west-2) | 4.78 s | **2.67 s** | 4.33 s |
| OpenAerialMap JPEG, overview (us-east-1) | 2.97 s | **2.30 s** | 4.35 s |
| OpenAerialMap JPEG, 2048² window (us-east-1) | 3.22 s | 2.66 s | **2.06 s** |
| ESA WorldCover, 4096² window (eu-central-1) | 0.62 s | **0.58 s** | 1.28 s |
| ESA WorldCover, overview (eu-central-1) | 0.53 s | **0.53 s** | 1.32 s |
| Copernicus DEM, whole file (eu-central-1) | **1.25 s** | 1.35 s | 1.66 s |

Local COGs (decode bound), first read: terrio-js is 1.1–6.6× faster (deflate 229 against 618 ms,
LZW 387 against 2410 ms, JPEG 565 against 1465 ms). Repeated reads of a window: 2–20 ms against
0.1–1.2 s, as terrio keeps decoded tiles.

Still behind: the OpenAerialMap window, as terrio reads every IFD at open (geotiff.js reads them
on demand).

## Zarr: terrio-js against zarrita 0.7.5

Local stores, 5 runs each:

| Case | main | branch | zarrita | zarrita tuned |
|---|---|---|---|---|
| v3 zstd 40 MB, first read | 183 ms | **121 ms** | 144 ms | 146 ms |
| v3 zstd 40 MB, warm | 93 ms | 61 ms | 61 ms | **39.5 ms** |
| v3 zstd 512² window, first / warm | 26 / 12 ms | **24** / 10.5 ms | 53 / 10 ms | 50 / **6.4** ms |
| v2 zlib 40 MB, first / warm | 171 / 80 ms | **106 / 49 ms** | 184 / 86 ms | 173 / 62 ms |
| v2 blosc, first / warm | 12 / 2.8 ms | **11 / 1.1 ms** | 49 / 4 ms | 53 / 2.2 ms |

TESSERA embeddings (sharded v3, blosc zstd + bitshuffle, S3 us-west-2), 128 bands of a 192 × 192
window:

| | main | branch | zarrita | zarrita tuned |
|---|---|---|---|---|
| Node.js open | 2.09 s | 1.84 s | **1.44 s** | 1.44 s |
| Node.js first read | 4.86 s | **2.30 s** | 2.46 s | 4.12 s |
| Node.js warm read | 140 ms | 40 ms | 380 ms | **25 ms** |
| Chromium open | 1.84 s | 1.57 s | **1.09 s** | 1.07 s |
| Chromium first read | 4.45 s | 4.76 s | 15.15 s | 4.84 s |
| Chromium warm read | 140 ms | 38 ms | 44 ms | **22 ms** |

"Tuned" is zarrita with `withRangeCoalescing` and `withByteCaching`. The Chromium first read of the
branch does not differ measurably from main's (their ranges overlap over 9 runs).

Remaining gaps:

- The v3 zstd warm read: `ruzstd` is ~40 ms of terrio's 61 ms. It is kept for now (pure Rust); a C
  libzstd for wasm32 is the open alternative.
- The open: terrio's reads the root group (TESSERA's is 64 KB; it holds the GeoZarr geotransform)
  and the band coordinates, which zarrita does not. The coordinates wait for the root, as the
  store's probing (v2 or v3, consolidated metadata) depends on it: one round trip, ~0.3 s.

## GeoParquet: terrio-js against hyparquet 1.31.5

terrio: `read()` then `toGeoJSON()`; hyparquet: `parquetReadObjects`, geometries decoded to GeoJSON.

| Case | main | branch | hyparquet |
|---|---|---|---|
| fiboa DE whole (90k polygons, brotli, 1 row group) | 6.9–7.0 s, 2.4 GB | **2.69 s**, 1.2 GB | 2.6–3.1 s, 1.6 GB |
| Overture bathymetry whole (407k, zstd, nested lists) | 8.6 s | **4.9 s** | 8.2–14 s, 4.4 GB |
| fiboa ES bbox (38k of 664k rows) | 3.55 s | **2.6 s** | 15–16 s, 6 GB |
| fiboa ES `id` + geometry (664k) | crash (wasm OOM) | **9.5 s** | 11.0 s |
| fiboa ES whole (664k) | crash (wasm OOM) | **10.0 s** | 14.7 s |
| Overture buildings bbox, S3 us-west-2 (5 runs, interleaved) | 6.4 s | **5.9 s** | 7.6 s |
| same, columns `id`, `height` | 6.0 s | **5.8 s** | 6.5 s |

The gap was not Parquet decoding (terrio's brotli is faster than hyparquet's JS one) but WKB to
geometries, a copy of the whole layer, and `toGeoJSON`. Not measured yet: Chromium, a release
build, `toArrowIPC` against `parquetReadObjects`.

## Size: a Zarr-only module (spike)

wasm-pack release builds (`opt-level = "s"`, fat LTO, `opt-level = 3` for the codec crates),
`wasm-opt -Oz`, brotli quality 11:

| Build | Raw | Brotli |
|---|---|---|
| terrio-js with only `--features zarr` (every export) | 22 MB | 4.1 MB |
| Only terrio's Zarr open and window read exported | 5.1 MB | 1.46 MB |
| The same, without the CRS engine and reading the Zarr source directly (not through `LazyRaster`) | **427 KB** | **154 KB** |
| zarrita 0.7.5 + numcodecs, bundled with esbuild | 1.45 MB | 337 KB |

- In the 1.46 MB module: ~2.4 MB raw of CRS (1.2 MB of ESRI WKT strings, the EPSG catalog in
  `wbprojection`, `terrio_crs`) and ~600 KB of TIFF codecs (JPEG XL, WebP, JPEG) that `LazyRaster`
  dispatches to. The Zarr driver is 147 KB, `ruzstd` 47 KB, `serde_json` 46 KB.
- The 154 KB module reads the local bench stores with equal checksums; its read path was not tuned.
- Go: the size target holds with terrio's own driver. A separate crate may not be needed: a feature
  turning the CRS engine off in the Zarr driver (raw attributes and an EPSG code kept) and a Zarr
  read entry point that skips `LazyRaster` would reach the same size.

## Tried and dropped

- Zarr zstd through `ruzstd::FrameDecoder::decode_all_to_vec` instead of a `StreamingDecoder`
  read: no gain (it zero-fills the whole buffer first).
- Zarr open: running the JS open again as each object arrives, so that the coordinates are asked
  for during the root's download. No gain: the store's opening stops on the missing root first.
- Splitting shard reads into more requests in Chromium: slower (6 connections to S3), so the split
  runs only in Node.js, Deno and Bun.

## Next

1. `terrio-js/zarr` for real: the CRS feature gate, a direct Zarr read entry point, a JS fetch
   loop; then tune its read path against zarrita.
2. Other formats against their JS libraries: COPC (copc.js + laz-perf), FlatGeobuf (`flatgeobuf`),
   CRS (proj4js), PMTiles last.
3. Land the branches upstream (one pull request each), when asked.

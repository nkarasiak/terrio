//! Reduction of raster bands over a zone index, strip by strip. Each zone's pixels are visited in
//! row-major order whatever the strip height, so results do not depend on it.

use super::Statistic;
use super::index::ZoneIndex;
use crate::raster::RasterData;
use crate::{DataType, Error, FieldValue, LazyRaster, Result, Window, WindowOptions};

/// A band to reduce: its number (from 1) and nodata value.
pub(super) struct BandSpec {
    pub band: usize,
    pub nodata: Option<f64>,
}

/// The weighting raster of a source, read alongside it.
pub(super) struct WeightSpec<'a> {
    pub raster: &'a mut LazyRaster,
    pub band: usize,
    pub nodata: Option<f64>,
}

/// What every reduction computes, with the questions the per-pixel loop asks answered once.
pub(super) struct Plan<'a> {
    stats: &'a [Statistic],
    fractional: bool,
    /// Feature IDs in layer order, for error messages.
    fids: &'a [u64],
    /// Whether some statistic needs the zone's values sorted.
    keep_values: bool,
    /// Whether some statistic needs the variance (a running mean and M2 per pixel).
    moments: bool,
    weighted_sum: bool,
    weighted_mean: bool,
    weighted_stddev: bool,
}

impl<'a> Plan<'a> {
    /// Bytes kept per pixel and band until a zone finishes: its value, and its coverage in
    /// fractional mode; none when no statistic keeps values.
    pub fn value_bytes_per_pixel(&self) -> usize {
        if self.keep_values { 8 * (1 + usize::from(self.fractional)) } else { 0 }
    }

    pub fn new(stats: &'a [Statistic], fractional: bool, fids: &'a [u64]) -> Self {
        Self {
            stats,
            fractional,
            fids,
            keep_values: stats.iter().any(|s| s.needs_values()),
            moments: stats.iter().any(|s| matches!(s, Statistic::StdDev | Statistic::Variance | Statistic::Cv)),
            weighted_sum: stats.contains(&Statistic::WeightedSum),
            weighted_mean: stats.contains(&Statistic::WeightedMean),
            weighted_stddev: stats.contains(&Statistic::WeightedStdDev),
        }
    }

    fn empty(&self) -> Vec<FieldValue> {
        let blank = Accumulator::default();
        self.stats.iter().map(|&s| blank.stat(s, self, &[])).collect()
    }
}

/// The windows read for `index` in strips of `strip_rows` rows from row 0: each strip holding
/// zone pixels, over the rows and columns of the pixels in it. Strips without zone pixels, such
/// as those between the parts of a multipolygon, are left out.
pub(super) fn strip_windows(index: &ZoneIndex, strip_rows: usize) -> Vec<Window> {
    // (strip, first row, end row, first column, end column) of the zone pixels in a strip, an
    // entry per zone and strip it crosses, merged by strip: memory follows the zones, not the
    // rows of the grid between them.
    let mut parts: Vec<(usize, usize, usize, usize, usize)> = Vec::new();
    for zone in &index.zones {
        let zone_parts = parts.len();
        for span in &zone.spans {
            let (row, col, end) = (span.row as usize, span.col as usize, (span.col + span.len) as usize);
            match parts[zone_parts..].last_mut() {
                // A zone's spans are in row-major order: its strips come one after another.
                Some(p) if p.0 == row / strip_rows => *p = (p.0, p.1.min(row), p.2.max(row + 1), p.3.min(col), p.4.max(end)),
                _ => parts.push((row / strip_rows, row, row + 1, col, end)),
            }
        }
    }
    parts.sort_unstable_by_key(|p| p.0);
    parts
        .chunk_by(|a, b| a.0 == b.0)
        .map(|strip| {
            let (r0, r1, c0, c1) =
                strip.iter().fold((usize::MAX, 0, usize::MAX, 0), |e, p| (e.0.min(p.1), e.1.max(p.2), e.2.min(p.3), e.3.max(p.4)));
            Window::new(c0, r0, c1 - c0, r1 - r0)
        })
        .collect()
}

/// The most pixels of the zones open at once while strips of `strip_rows` rows are reduced, each
/// zone counting at most `per_zone` ([`values_per_zone`]): what each band's value lists hold at
/// their peak, as `reduce_source` opens a zone at the strip of its first row and finishes it after
/// the strip of its last.
pub(super) fn open_pixels_peak(index: &ZoneIndex, strip_rows: usize, per_zone: usize) -> usize {
    let strip_rows = strip_rows.max(1);
    // (strip, change in open pixels): two per zone, so memory follows the zones, not the grid.
    let mut changes: Vec<(usize, i64)> = index
        .zones
        .iter()
        .flat_map(|zone| {
            let pixels = zone.spans.iter().map(|s| s.len as usize).sum::<usize>().min(per_zone) as i64;
            [(zone.row_start / strip_rows, pixels), ((zone.row_end - 1) / strip_rows + 1, -pixels)]
        })
        .collect();
    changes.sort_unstable_by_key(|c| c.0);
    let mut open = 0i64;
    changes
        .chunk_by(|a, b| a.0 == b.0)
        .map(|strip| {
            open += strip.iter().map(|c| c.1).sum::<i64>();
            open
        })
        .max()
        .unwrap_or(0)
        .max(0) as usize
}

/// Statistics of each band (in `bands` order) for each statistic (in plan order) and feature
/// (in layer order). Strips are `strip_rows` rows starting at multiples of it; strips that no
/// zone intersects are not read. The next strips are read on other threads while one is reduced
/// (`strips::read_ahead`).
pub(super) fn reduce_source(
    index: &ZoneIndex,
    raster: &mut LazyRaster,
    bands: &[BandSpec],
    weight: Option<WeightSpec<'_>>,
    plan: &Plan<'_>,
    strip_rows: usize,
    ahead: usize,
) -> Result<Vec<Vec<Vec<FieldValue>>>> {
    let windows = strip_windows(index, strip_rows.max(1));
    let read_bands = WindowOptions { bands: bands.iter().map(|b| b.band).collect(), overview: None };
    // Reads share one handle each: reads of a raster take turns, but overlap the reduction.
    let raster = std::sync::Mutex::new(raster);
    let weight_band = weight.as_ref().map(|w| (w.band, w.nodata));
    let weight_raster = std::sync::Mutex::new(weight.map(|w| w.raster));
    let lock = |m| Error::InvalidArgument(format!("zonal {m} reader lock poisoned"));
    let mut state = StreamState::new(bands.iter().map(|b| b.nodata).collect(), plan);
    crate::ops::strips::read_ahead_by(
        ahead,
        windows.len(),
        |i| {
            let window = windows[i];
            let strip = Strip::read(&mut **raster.lock().map_err(|_| lock("raster"))?, window, &read_bands)?;
            let weights = match (weight_band, &mut *weight_raster.lock().map_err(|_| lock("weight"))?) {
                (Some((band, nodata)), Some(w)) => {
                    Some((Strip::read(w, window, &WindowOptions { bands: vec![band], overview: None })?, nodata))
                }
                _ => None,
            };
            Ok((strip, weights))
        },
        |i, (strip, weights)| state.add_strip(index, plan, windows[i], &strip, weights.as_ref().map(|(w, nodata)| (w, *nodata))),
    )?;
    state.finish(index)
}

/// A reduction fed strip by strip in increasing rows: a zone opens at the strip holding its first
/// row and is summarized at the strip holding its last, its pixels added in row-major order.
pub(super) struct StreamState {
    /// Per band.
    nodata: Vec<Option<f64>>,
    /// Open zones: (zone, next span, one accumulator per band).
    open: Vec<(usize, usize, Vec<Accumulator>)>,
    /// The next zone to open.
    next: usize,
    /// `[band][statistic][feature]`, empty values until a zone is summarized.
    out: Vec<Vec<Vec<FieldValue>>>,
}

impl StreamState {
    pub fn new(nodata: Vec<Option<f64>>, plan: &Plan<'_>) -> Self {
        let features = plan.fids.len();
        let empty = plan.empty();
        let out = nodata.iter().map(|_| empty.iter().map(|v| vec![v.clone(); features]).collect()).collect();
        Self { nodata, open: Vec::new(), next: 0, out }
    }

    /// Adds `strip`, the pixels of `window` (band after band), with the weights of the same window
    /// and their nodata. Windows must come in increasing rows, as [`strip_windows`] gives them.
    pub fn add_strip(
        &mut self,
        index: &ZoneIndex,
        plan: &Plan<'_>,
        window: Window,
        strip: &Strip<'_>,
        weights: Option<(&Strip<'_>, Option<f64>)>,
    ) -> Result<()> {
        let zones = &index.zones;
        let Window { x: c0, y: r0, height, .. } = window;
        let r1 = r0 + height;
        let bands = self.nodata.len();
        while self.next < zones.len() && zones[self.next].row_start < r1 {
            // Value lists hold at most the zone's pixels, or `HISTOGRAM_BINS` of an 8- or 16-bit
            // band: reserved once.
            let pixels = if plan.keep_values {
                zones[self.next].spans.iter().map(|s| s.len as usize).sum::<usize>().min(values_per_zone(strip.data_type))
            } else {
                0
            };
            let accumulator = || Accumulator {
                values: Vec::with_capacity(pixels),
                coverages: Vec::with_capacity(if plan.fractional { pixels } else { 0 }),
                ..Default::default()
            };
            self.open.push((self.next, 0, (0..bands).map(|_| accumulator()).collect()));
            self.next += 1;
        }
        // Zones accumulate independently: in parallel, keeping the error of the first zone.
        let failure = std::sync::Mutex::new(None::<(usize, Error)>);
        let nodata = &self.nodata;
        let chunk = crate::par::chunk_len(self.open.len(), 8);
        crate::par::chunks_mut(&mut self.open, chunk, |_, chunk| {
            for (zone, cursor, accumulators) in chunk {
                let spans = &zones[*zone].spans;
                let fid = plan.fids[zones[*zone].feature];
                let mut run = || -> Result<()> {
                    while let Some(span) = spans.get(*cursor).filter(|s| (s.row as usize) < r1) {
                        // Band by band, the span's pixels in a loop of their sample type: a
                        // band's pixels are still added in row-major order.
                        let start = (span.row as usize - r0) * strip.cols + span.col as usize - c0;
                        let (len, coverage, data_type) = (span.len as usize, span.coverage, strip.data_type);
                        for (b, (acc, &nodata)) in accumulators.iter_mut().zip(nodata).enumerate() {
                            match weights {
                                // Nothing to fail on without weights.
                                None => strip.each(b, start, len, |_, v| {
                                    acc.add_value(v, nodata, data_type, coverage, plan);
                                    Ok(())
                                })?,
                                Some((w, w_nodata)) => strip.each(b, start, len, |i, v| {
                                    let w = (w.value(0, start + i), w_nodata, w.data_type);
                                    acc.add(v, nodata, data_type, coverage, w, plan, fid)
                                })?,
                            }
                        }
                        *cursor += 1;
                    }
                    Ok(())
                };
                if let Err(e) = run() {
                    if let Ok(mut failure) = failure.lock()
                        && failure.as_ref().is_none_or(|(first, _)| *zone < *first)
                    {
                        *failure = Some((*zone, e));
                    }
                    return;
                }
            }
        });
        if let Ok(Some((_, e))) = failure.into_inner() {
            return Err(e);
        }
        // Finished zones: binned and summarized in parallel, then written.
        let (finished, still_open): (Vec<_>, Vec<_>) = std::mem::take(&mut self.open).into_iter().partition(|o| zones[o.0].row_end <= r1);
        self.open = still_open;
        let mut jobs: Vec<_> = finished.into_iter().map(|o| (o, Vec::new())).collect();
        let chunk = crate::par::chunk_len(jobs.len(), 4);
        crate::par::chunks_mut(&mut jobs, chunk, |_, chunk| {
            for ((_, _, accumulators), values) in chunk {
                *values = accumulators
                    .iter_mut()
                    .map(|acc| {
                        let bins = if plan.keep_values { acc.bins(plan.fractional) } else { Vec::new() };
                        plan.stats.iter().map(|&stat| acc.stat(stat, plan, &bins)).collect::<Vec<_>>()
                    })
                    .collect();
            }
        });
        for ((zone, _, _), values) in jobs {
            let feature = zones[zone].feature;
            for (b, band_values) in values.into_iter().enumerate() {
                for (s, value) in band_values.into_iter().enumerate() {
                    self.out[b][s][feature] = value;
                }
            }
        }
        Ok(())
    }

    /// `[band][statistic][feature]`; an error unless every zone of `index` was summarized.
    pub fn finish(self, index: &ZoneIndex) -> Result<Vec<Vec<Vec<FieldValue>>>> {
        let unfinished = self.open.len() + index.zones.len() - self.next;
        if unfinished > 0 {
            return Err(Error::InvalidArgument(format!("zonal statistics: missing windows, {unfinished} zones not finished")));
        }
        Ok(self.out)
    }
}

/// A window of some bands, band after band.
pub(super) struct Strip<'a> {
    data: std::borrow::Cow<'a, RasterData>,
    cols: usize,
    pixels: usize,
    data_type: DataType,
}

impl Strip<'static> {
    fn read(raster: &mut LazyRaster, window: Window, options: &WindowOptions) -> Result<Self> {
        Ok(Strip::of(std::borrow::Cow::Owned(raster.read_window(window, options)?.raster.data), window))
    }
}

impl<'a> Strip<'a> {
    /// `data`, the pixels of `window` band after band, row-major.
    pub fn of(data: std::borrow::Cow<'a, RasterData>, window: Window) -> Self {
        let data_type = data.data_type();
        Self { data, cols: window.width, pixels: window.width * window.height, data_type }
    }

    fn value(&self, band: usize, at: usize) -> f64 {
        self.data.get_f64(band * self.pixels + at)
    }

    /// Calls `f` with the offset from `at` and the value of each of `len` pixels of `band` from
    /// `at`: one loop per sample type, into which `f` inlines. Reading values one by one
    /// (`value`, a call and a type dispatch each) made the reduction about five times slower.
    #[inline]
    fn each(&self, band: usize, at: usize, len: usize, mut f: impl FnMut(usize, f64) -> Result<()>) -> Result<()> {
        use RasterData as D;
        let from = band * self.pixels + at;
        macro_rules! each {
            ($($variant:ident),*) => {
                match &*self.data {
                    $(D::$variant(v) => {
                        for (i, &x) in v[from..from + len].iter().enumerate() {
                            f(i, x as f64)?;
                        }
                    })*
                }
            };
        }
        each!(U8, I8, U16, I16, U32, I32, U64, I64, F32, F64);
        Ok(())
    }
}

/// One zone's running statistics for one band.
#[derive(Default)]
struct Accumulator {
    summary: Summary,
    weighted: Summary,
    invalid_weight: bool,
    /// Values, kept when a statistic needs them sorted.
    values: Vec<f64>,
    /// Their coverages, in fractional mode (binary coverage is 1).
    coverages: Vec<f64>,
    /// Once a zone holds `HISTOGRAM_BINS` values of an 8- or 16-bit band, the weight (pixel count,
    /// or coverage sum in fractional mode) of each integer from `HISTOGRAM_FIRST`: it takes those
    /// values from then on, the lists only the others. Empty until then.
    histogram: Vec<f64>,
}

/// The integers a histogram counts, from `HISTOGRAM_FIRST`: every value of an 8- or 16-bit band.
const HISTOGRAM_BINS: usize = 98_304;
const HISTOGRAM_FIRST: f64 = -32_768.0;

/// The most values an accumulator lists for a zone of a band of `data_type`: `HISTOGRAM_BINS`
/// for 8- and 16-bit integers, which a histogram counts past that.
pub(super) fn values_per_zone(data_type: DataType) -> usize {
    if counted(data_type) { HISTOGRAM_BINS } else { usize::MAX }
}

fn counted(data_type: DataType) -> bool {
    matches!(data_type, DataType::U8 | DataType::I8 | DataType::U16 | DataType::I16)
}

/// The histogram bin of `v`, if it counts it: an integer from `HISTOGRAM_FIRST` (-0 counts as 0).
#[inline]
fn histogram_bin(v: f64) -> Option<usize> {
    // Saturating: below the first bin is 0, NaN is 0, and either fails the round trip.
    let bin = (v - HISTOGRAM_FIRST) as usize;
    (bin < HISTOGRAM_BINS && HISTOGRAM_FIRST + bin as f64 == v).then_some(bin)
}

/// Equal values of `values` merged as `(value, count)` in increasing order.
fn counts(mut values: Vec<f64>) -> Vec<(f64, f64)> {
    // Integers the histogram counts (but -0, which `total_cmp` sorts apart: the round trip of its
    // bits fails), as 17-bit keys sorted by radix, faster than comparing the values.
    let key = |v: f64| (v - HISTOGRAM_FIRST) as u32;
    let counted = |v: f64| (key(v) as usize) < HISTOGRAM_BINS && (HISTOGRAM_FIRST + f64::from(key(v))).to_bits() == v.to_bits();
    if values.iter().all(|&v| counted(v)) {
        let mut keys: Vec<u32> = values.iter().map(|&v| key(v)).collect();
        drop(values);
        radix_sort(&mut keys);
        return keys.chunk_by(|a, b| a == b).map(|run| (HISTOGRAM_FIRST + run[0] as f64, run.len() as f64)).collect();
    }
    values.sort_unstable_by(f64::total_cmp);
    values.chunk_by(|a, b| a == b).map(|run| (run[0], run.len() as f64)).collect()
}

/// Sorts keys below 2^17 (histogram bins): a least-significant-digit radix sort of 9 then 8 bits.
fn radix_sort(keys: &mut Vec<u32>) {
    const _: () = assert!(HISTOGRAM_BINS <= 1 << 17);
    let mut sorted = vec![0u32; keys.len()];
    for (shift, bits) in [(0, 9), (9, 8)] {
        let digit = |k: u32| ((k >> shift) & ((1 << bits) - 1)) as usize;
        let mut next = [0usize; 1 << 9];
        for &k in keys.iter() {
            next[digit(k)] += 1;
        }
        let mut start = 0;
        for n in &mut next {
            (*n, start) = (start, start + *n);
        }
        for &k in keys.iter() {
            let d = digit(k);
            sorted[next[d]] = k;
            next[d] += 1;
        }
        std::mem::swap(keys, &mut sorted);
    }
}

impl Accumulator {
    #[allow(clippy::too_many_arguments)]
    #[inline]
    fn add(
        &mut self,
        v: f64,
        nodata: Option<f64>,
        data_type: DataType,
        coverage: f64,
        (w, w_nodata, w_type): (f64, Option<f64>, DataType),
        plan: &Plan<'_>,
        fid: u64,
    ) -> Result<()> {
        if !self.add_value(v, nodata, data_type, coverage, plan) {
            return Ok(());
        }
        if !w.is_finite() || !crate::histogram::is_valid(w, w_nodata, w_type) {
            self.invalid_weight = true;
        } else if w < 0.0 {
            return Err(Error::InvalidArgument(format!("zone {fid} contains a negative weight")));
        } else if w > 0.0 {
            let contribution = coverage * w;
            if contribution > 0.0 {
                self.weighted.add(v, contribution, true);
                let m2 = plan.weighted_stddev;
                if (plan.weighted_sum && !self.weighted.sum.is_finite())
                    || ((m2 || plan.weighted_mean) && (!self.weighted.weight.is_finite() || !self.weighted.mean.is_finite()))
                    || (m2 && !self.weighted.m2.is_finite())
                {
                    return Err(Error::InvalidArgument(format!("zone {fid} weighted statistics overflow binary64 arithmetic")));
                }
            }
        }
        Ok(())
    }

    /// Adds a value without weight, unless it is nodata or not finite: whether it counts.
    #[inline]
    fn add_value(&mut self, v: f64, nodata: Option<f64>, data_type: DataType, coverage: f64, plan: &Plan<'_>) -> bool {
        if !(v.is_finite() && crate::histogram::is_valid(v, nodata, data_type)) {
            return false;
        }
        self.summary.add(v, coverage, plan.moments);
        if plan.keep_values {
            if !self.histogram.is_empty()
                && let Some(bin) = histogram_bin(v)
            {
                self.histogram[bin] += if plan.fractional { coverage } else { 1.0 };
                return true;
            }
            self.values.push(v);
            if plan.fractional {
                self.coverages.push(coverage);
            }
            if self.values.len() >= HISTOGRAM_BINS && self.histogram.is_empty() && counted(data_type) {
                self.start_histogram(plan.fractional);
            }
        }
        true
    }

    /// Moves the listed values the histogram counts into it, in the order they were added.
    fn start_histogram(&mut self, fractional: bool) {
        let mut histogram = vec![0.0; HISTOGRAM_BINS];
        let mut kept = 0;
        for i in 0..self.values.len() {
            let v = self.values[i];
            match histogram_bin(v) {
                Some(bin) => histogram[bin] += if fractional { self.coverages[i] } else { 1.0 },
                None => {
                    self.values[kept] = v;
                    if fractional {
                        self.coverages[kept] = self.coverages[i];
                    }
                    kept += 1;
                }
            }
        }
        self.values.truncate(kept);
        self.values.shrink_to_fit();
        self.coverages.truncate(kept);
        self.coverages.shrink_to_fit();
        self.histogram = histogram;
    }

    /// The kept values as `(value, weight)` bins of equal values in increasing order, the weight
    /// being their pixel count, or in fractional mode their coverage sum; the lists and histogram
    /// are emptied. Coverages are summed in sorted order (unstable) in the lists, in the order
    /// they were added in the histogram, so their rounding can differ.
    fn bins(&mut self, fractional: bool) -> Vec<(f64, f64)> {
        let values = std::mem::take(&mut self.values);
        let listed: Vec<(f64, f64)> = if fractional {
            let mut pairs: Vec<(f64, f64)> = values.into_iter().zip(std::mem::take(&mut self.coverages)).collect();
            pairs.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
            let mut bins: Vec<(f64, f64)> = Vec::new();
            for (v, coverage) in pairs {
                match bins.last_mut() {
                    Some(last) if last.0 == v => last.1 += coverage,
                    _ => bins.push((v, coverage)),
                }
            }
            bins
        } else {
            counts(values)
        };
        let histogram = std::mem::take(&mut self.histogram);
        if histogram.is_empty() {
            return listed;
        }
        // The listed values are those the histogram does not count: no value is in both.
        let mut listed = listed.into_iter().peekable();
        let mut bins = Vec::new();
        for (i, &weight) in histogram.iter().enumerate().filter(|b| *b.1 > 0.0) {
            let v = HISTOGRAM_FIRST + i as f64;
            bins.extend(std::iter::from_fn(|| listed.next_if(|l| l.0 < v)));
            bins.push((v, weight));
        }
        bins.extend(listed);
        bins
    }

    /// `bins` is [`Accumulator::bins`] when the statistic needs values.
    fn stat(&self, stat: Statistic, plan: &Plan<'_>, bins: &[(f64, f64)]) -> FieldValue {
        if stat.uses_weights() {
            return if self.invalid_weight || self.summary.count == 0 {
                FieldValue::Null
            } else if stat == Statistic::WeightedSum {
                FieldValue::Float(self.weighted.sum)
            } else if self.weighted.weight == 0.0 {
                FieldValue::Null
            } else {
                FieldValue::Float(if stat == Statistic::WeightedMean {
                    self.weighted.mean
                } else {
                    (self.weighted.m2 / self.weighted.weight).max(0.0).sqrt()
                })
            };
        }
        self.summary.stat(stat, bins, plan.fractional)
    }
}

#[derive(Default)]
struct Summary {
    count: usize,
    weight: f64,
    sum: f64,
    mean: f64,
    m2: f64,
    min: f64,
    max: f64,
}

impl Summary {
    /// Adds `value` with `weight`; with `moments`, also to the running mean and M2 (Welford's
    /// algorithm) that the variance needs. Without, no division depends on the previous pixel's:
    /// the mean is the sum over the weight, as exactextract computes it.
    #[inline]
    fn add(&mut self, value: f64, weight: f64, moments: bool) {
        if self.count == 0 {
            self.min = value;
            self.max = value;
        }
        self.count += 1;
        self.weight += weight;
        self.sum += value * weight;
        if moments {
            let delta = value - self.mean;
            // Preserve the existing unweighted delta/count arithmetic when each pixel has weight one.
            self.mean += if weight == 1.0 { delta / self.weight } else { (weight / self.weight) * delta };
            self.m2 += weight * (delta * (value - self.mean));
        }
        self.min = self.min.min(value);
        self.max = self.max.max(value);
    }

    fn stat(&self, stat: Statistic, bins: &[(f64, f64)], fractional: bool) -> FieldValue {
        match stat {
            Statistic::Count => {
                return if fractional { FieldValue::Float(self.weight) } else { FieldValue::Integer(self.count as i64) };
            }
            Statistic::Variety => return FieldValue::Integer(bins.len() as i64),
            _ if self.count == 0 => return FieldValue::Null,
            _ => {}
        }
        let variance = (self.m2 / self.weight).max(0.0);
        let mean = self.sum / self.weight;
        FieldValue::Float(match stat {
            Statistic::Min => self.min,
            Statistic::Max => self.max,
            Statistic::Mean => mean,
            Statistic::Sum => self.sum,
            Statistic::StdDev => variance.sqrt(),
            Statistic::Variance => variance,
            Statistic::Cv if mean == 0.0 => return FieldValue::Null,
            Statistic::Cv => variance.sqrt() / mean,
            Statistic::Median => quantile(bins, 0.5, fractional, self.count),
            Statistic::Quantile(p) => quantile(bins, p / 100.0, fractional, self.count),
            // Bins ascend by value: `>=` lets a later (larger) value win a mode tie, `<` keeps the
            // earlier (smaller) value on a minority tie, as exactextract 0.3.0 does.
            Statistic::Mode => bins.iter().fold((f64::NAN, f64::NEG_INFINITY), |b, &(v, w)| if w >= b.1 { (v, w) } else { b }).0,
            Statistic::Minority => bins.iter().fold((f64::NAN, f64::INFINITY), |b, &(v, w)| if w < b.1 { (v, w) } else { b }).0,
            Statistic::Count | Statistic::Variety | Statistic::WeightedMean | Statistic::WeightedSum | Statistic::WeightedStdDev => {
                unreachable!()
            }
        })
    }
}

/// Quantile `q` (0 to 1) of the non-empty [`Accumulator::bins`] of `count` values: exactextract's
/// weighted quantile in fractional mode, else linear interpolation between the closest ranks
/// (numpy's default), with a halfway point taken by `f64::midpoint` so the 0.5 quantile is the
/// median of two middle values.
fn quantile(bins: &[(f64, f64)], q: f64, fractional: bool, count: usize) -> f64 {
    if fractional {
        return super::fractional::quantile(bins, q);
    }
    let h = (count - 1) as f64 * q;
    let lo = h.floor() as usize;
    let fraction = h - lo as f64;
    // The values of ranks `lo` and `lo + 1` (from 0), the second only when interpolated.
    let mut seen = 0.0;
    let at = bins.iter().position(|b| {
        seen += b.1;
        seen > lo as f64
    });
    let Some(at) = at else { return f64::NAN };
    let a = bins[at].0;
    let b = if seen > (lo + 1) as f64 { a } else { bins.get(at + 1).map_or(a, |b| b.0) };
    if fraction == 0.0 {
        a
    } else if fraction == 0.5 {
        a.midpoint(b)
    } else {
        // Values of opposite signs are weighted apart: their difference can overflow.
        if a.is_sign_negative() == b.is_sign_negative() { a + fraction * (b - a) } else { a * (1.0 - fraction) + b * fraction }
    }
}

#[cfg(test)]
mod tests {
    use super::super::index::{GridKey, Span, Zone, ZoneIndex, build};
    use super::{BandSpec, Plan, open_pixels_peak, reduce_source};

    #[test]
    fn open_pixels_peak_counts_zones_open_in_the_same_strip() {
        let zone = |row_start: usize, row_end: usize, len| Zone {
            feature: 0,
            row_start,
            row_end,
            spans: (row_start..row_end).map(|row| Span { row: row as u32, col: 0, len, coverage: 1.0 }).collect(),
        };
        // Strips of 10 rows: the zones of rows 0-14 (30 pixels) and 12-29 (18) are open together
        // in strip 1; the zone of row 40 (100) is alone.
        let zones = vec![zone(0, 15, 2), zone(12, 30, 1), zone(40, 41, 100)];
        assert_eq!(open_pixels_peak(&ZoneIndex { zones: zones.clone() }, 10, usize::MAX), 100);
        assert_eq!(open_pixels_peak(&ZoneIndex { zones: zones[..2].to_vec() }, 10, usize::MAX), 48);
        assert_eq!(open_pixels_peak(&ZoneIndex { zones: Vec::new() }, 10, usize::MAX), 0);
        // Each zone counted at most `per_zone` pixels: 20 + 18 together in strip 1.
        assert_eq!(open_pixels_peak(&ZoneIndex { zones: zones.clone() }, 10, 20), 38);
        // Counted over the zones, not the rows between them: here rows 0 and 4e9.
        assert_eq!(open_pixels_peak(&ZoneIndex { zones: vec![zone(0, 1, 5), zone(4_000_000_000, 4_000_000_002, 3)] }, 1, usize::MAX), 6);
    }
    use crate::ops::zonal::Statistic;
    use crate::vector::Coord;
    use crate::{DataType, Feature, FieldValue, Geometry, Layer, LazyRaster, Raster, RasterConfig, RasterDataset};

    fn ring(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<Coord> {
        vec![Coord::xy(x0, y0), Coord::xy(x1, y0), Coord::xy(x1, y1), Coord::xy(x0, y1), Coord::xy(x0, y0)]
    }

    #[test]
    fn strip_windows_cover_zone_pixels_and_skip_strips_without_them() {
        use super::super::index::{Span, Zone, ZoneIndex};
        use super::strip_windows;
        use crate::Window;
        // `rows` rows of `len` pixels from (`row`, `col`).
        let block = |row: u32, rows: u32, col, len| (row..row + rows).map(move |row| Span { row, col, len, coverage: 1.0 });
        let zone = |spans: Vec<Span>| Zone {
            feature: 0,
            row_start: spans[0].row as usize,
            row_end: spans[spans.len() - 1].row as usize + 1,
            spans,
        };
        // A multipart zone's parts, far apart: the strips between them are not read.
        let parts = block(300, 10, 1, 2).chain(block(1500, 10, 9, 31)).chain(block(1599, 1, 0, 1)).collect();
        let index = ZoneIndex { zones: vec![zone(block(0, 10, 5, 3).collect()), zone(parts)] };
        assert_eq!(
            strip_windows(&index, 256),
            [Window::new(5, 0, 3, 10), Window::new(1, 300, 2, 10), Window::new(9, 1500, 31, 10), Window::new(0, 1599, 1, 1)]
        );
        assert!(strip_windows(&ZoneIndex { zones: vec![] }, 256).is_empty());
        // Memory follows the zones, not the rows between them: zones at rows 0 and 4e9.
        let index = ZoneIndex { zones: vec![zone(block(0, 1, 1, 1).collect()), zone(block(4_000_000_007, 2, 2, 4).collect())] };
        assert_eq!(
            strip_windows(&index, 1),
            [Window::new(1, 0, 1, 1), Window::new(2, 4_000_000_007, 4, 1), Window::new(2, 4_000_000_008, 4, 1)]
        );
        // Zones sharing a strip give one window over both.
        let index = ZoneIndex { zones: vec![zone(block(0, 2, 1, 1).collect()), zone(block(1, 2, 5, 3).collect())] };
        assert_eq!(strip_windows(&index, 256), [Window::new(1, 0, 7, 3)]);
    }

    #[test]
    fn binary_quantiles_of_extreme_values_of_opposite_signs_stay_finite() {
        let sorted = [(-f64::MAX, 1.0), (f64::MAX, 1.0)];
        let q25 = super::quantile(&sorted, 0.25, false, 2);
        assert!((q25 / (-f64::MAX / 2.0) - 1.0).abs() < 1e-15, "{q25}");
        assert_eq!(super::quantile(&sorted, 0.5, false, 2), 0.0);
        assert_eq!(super::quantile(&[(1.0, 1.0), (3.0, 1.0)], 0.25, false, 2), 1.5);
    }

    #[test]
    fn zonal_mean_is_the_sum_over_the_count_whatever_else_is_requested() {
        let (cols, rows) = (37, 23);
        let values: Vec<f64> = (0..cols * rows).map(|i| 1e6 + ((i * 7919) % 101) as f64 / 7.).collect();
        let sum: f64 = values.iter().sum();
        let cfg = RasterConfig { cols, rows, data_type: DataType::F64, ..Default::default() };
        let mut ds = RasterDataset::new(Raster::from_data(cfg, values).unwrap(), false);
        ds.set_geo_transform([0.0, 1.0, 0.0, rows as f64, 0.0, -1.0]);
        let mut layer = Layer::new("zones");
        layer.features.push(Feature {
            fid: 0,
            geometry: Some(Geometry::polygon(ring(0., 0., cols as f64, rows as f64), vec![])),
            attributes: vec![],
        });
        let mut raster = LazyRaster::from(ds);
        let index = build(&layer, &GridKey::of(&raster), false, false).unwrap();
        let mut mean = |names: &[&str]| {
            let stats: Vec<Statistic> = names.iter().map(|s| Statistic::parse(s).unwrap()).collect();
            let plan = Plan::new(&stats, false, &[0]);
            reduce_source(&index, &mut raster, &[BandSpec { band: 1, nodata: None }], None, &plan, rows, 4).unwrap()[0][0][0].clone()
        };
        let expected = crate::FieldValue::Float(sum / (cols * rows) as f64);
        assert_eq!(mean(&["mean"]), expected);
        assert_eq!(mean(&["mean", "stddev", "cv"]), expected);
    }

    /// Zones of more than `HISTOGRAM_BINS` pixels of 8- and 16-bit bands are counted in a
    /// histogram: the same results as the value lists of the same values in a Float64 band.
    #[test]
    fn counted_integer_bands_match_listed_float_bands() {
        let (cols, rows) = (400, 300);
        assert!(cols * rows > super::HISTOGRAM_BINS);
        let mut layer = Layer::new("zones");
        let geometries = [
            Geometry::polygon(ring(0., 0., cols as f64, rows as f64), vec![]),
            Geometry::polygon(ring(3.3, 2.7, 390.6, 290.2), vec![ring(50.5, 50.5, 60.2, 70.9)]),
            Geometry::polygon(ring(10.5, 10.5, 30.5, 40.5), vec![]),
        ];
        for (i, g) in geometries.into_iter().enumerate() {
            layer.features.push(Feature { fid: i as u64, geometry: Some(g), attributes: vec![] });
        }
        let names = "count min max mean median quantile_0 quantile_10 quantile_33 quantile_90 quantile_100 mode minority variety";
        let stats: Vec<Statistic> = names.split(' ').map(|s| Statistic::parse(s).unwrap()).collect();
        let fids = [0, 1, 2];
        for (data_type, first, values) in
            [(DataType::U8, 0, 256), (DataType::I8, -128, 256), (DataType::U16, 0, 65536), (DataType::I16, -32768, 65536)]
        {
            // Skewed, with ties, and every value of the type.
            let pixels: Vec<f64> =
                (0..cols * rows).map(|i| (first + ((i as i64 * i as i64 * 7919 + i as i64 / 3) % values)) as f64).collect();
            let raster = |data_type| {
                let cfg = RasterConfig { cols, rows, data_type, ..Default::default() };
                let mut ds = RasterDataset::new(Raster::from_data(cfg, pixels.clone()).unwrap(), false);
                ds.set_geo_transform([0.0, 1.0, 0.0, rows as f64, 0.0, -1.0]);
                LazyRaster::from(ds)
            };
            let (mut counted, mut listed) = (raster(data_type), raster(DataType::F64));
            assert_eq!(counted.data_type(), data_type);
            let nodata = Some((first + 5) as f64);
            for fractional in [false, true] {
                let index = build(&layer, &GridKey::of(&listed), false, fractional).unwrap();
                let plan = Plan::new(&stats, fractional, &fids);
                let expected = reduce_source(&index, &mut listed, &[BandSpec { band: 1, nodata }], None, &plan, rows, 1).unwrap();
                assert!(expected[0].iter().all(|s| s.iter().all(|v| !v.is_null())));
                for strip in [rows, 7] {
                    let got = reduce_source(&index, &mut counted, &[BandSpec { band: 1, nodata }], None, &plan, strip, 2).unwrap();
                    if !fractional {
                        assert_eq!(got, expected, "{data_type:?} strip {strip}");
                        continue;
                    }
                    // Coverage sums are rounded in another order (`Accumulator::bins`).
                    for (g, e) in got.iter().flatten().flatten().zip(expected.iter().flatten().flatten()) {
                        match (g, e) {
                            (FieldValue::Float(g), FieldValue::Float(e)) => {
                                assert!((g - e).abs() <= 1e-12 * e.abs(), "{data_type:?} strip {strip}: {g} {e}")
                            }
                            _ => assert_eq!(g, e, "{data_type:?} strip {strip}"),
                        }
                    }
                }
            }
        }
    }

    /// A zone fed windows of several sample types (a `ZonalStream`) once its histogram is
    /// started: values it does not count (fractions, out of range) stay listed.
    #[test]
    fn histogram_keeps_listing_values_it_does_not_count() {
        use super::{Accumulator, HISTOGRAM_BINS};
        let stats: Vec<Statistic> = ["median", "quantile_0", "quantile_25", "quantile_99", "quantile_100", "mode", "minority", "variety"]
            .iter()
            .map(|s| Statistic::parse(s).unwrap())
            .collect();
        let others = [2.5, 70_000.0, -40_000.0, -3.0, 1e9, 2.5, 7.0, -0.5, 65_535.0, -32_768.0, 70_000.0];
        for fractional in [false, true] {
            let plan = Plan::new(&stats, fractional, &[0]);
            let coverage = |i: usize| if fractional { [1.0, 0.25, 0.5, 0.125][i % 4] } else { 1.0 };
            let (mut counted, mut listed) = (Accumulator::default(), Accumulator::default());
            for i in 0..HISTOGRAM_BINS + 1000 {
                let v = ((i * i) % 1000) as f64;
                counted.add_value(v, None, DataType::U16, coverage(i), &plan);
                listed.add_value(v, None, DataType::F64, coverage(i), &plan);
            }
            assert!(!counted.histogram.is_empty() && listed.histogram.is_empty());
            for (i, &v) in others.iter().enumerate() {
                counted.add_value(v, None, DataType::F64, coverage(i), &plan);
                listed.add_value(v, None, DataType::F64, coverage(i), &plan);
            }
            assert_eq!(counted.values.len(), 7);
            // Coverages are exact binary fractions here: their sums do not depend on the order.
            let (c, l) = (counted.bins(fractional), listed.bins(fractional));
            assert_eq!(c, l, "fractional {fractional}");
            for &stat in &stats {
                assert_eq!(counted.stat(stat, &plan, &c), listed.stat(stat, &plan, &l), "{stat:?} fractional {fractional}");
            }
        }
    }

    /// Integers counted by radix sort, other values compared: the same counts in the same order.
    #[test]
    fn counts_sort_integers_by_radix_like_any_values() {
        let reference = |values: &[f64]| {
            let mut sorted = values.to_vec();
            sorted.sort_unstable_by(f64::total_cmp);
            sorted.chunk_by(|a, b| a == b).map(|run| (run[0], run.len() as f64)).collect::<Vec<_>>()
        };
        let integers: Vec<f64> = (0..20_000u64).map(|i| ((i * i * 7919 + i) % 98_304) as f64 - 32_768.0).collect();
        assert!(integers.contains(&-32_768.0) && integers.iter().any(|&v| v > 60_000.0));
        let cases = [
            integers.clone(),
            [integers.clone(), vec![65_535.0, -32_768.0, 0.0, 0.0]].concat(),
            // Not all counted: compared, -0 apart from 0 as `total_cmp` sorts it, merged by `==`.
            [integers.clone(), vec![0.5]].concat(),
            vec![0.0, -0.0, 3.0, -0.0],
            vec![70_000.0, 1.0, 1.0],
            vec![],
        ];
        for values in cases {
            let got = super::counts(values.clone());
            let expected = reference(&values);
            assert_eq!(got.len(), expected.len());
            for (g, e) in got.iter().zip(&expected) {
                assert_eq!((g.0.to_bits(), g.1), (e.0.to_bits(), e.1));
            }
        }
    }

    #[test]
    fn zonal_strip_height_does_not_change_results() {
        let (cols, rows) = (40, 300);
        let values: Vec<f64> = (0..2 * cols * rows).map(|i| ((i * 7919) % 97) as f64 / 3.).collect();
        let cfg = RasterConfig { cols, rows, bands: 2, data_type: DataType::F64, ..Default::default() };
        let mut ds = RasterDataset::new(Raster::from_data(cfg, values).unwrap(), false);
        ds.set_geo_transform([0.0, 1.0, 0.0, rows as f64, 0.0, -1.0]);
        let mut layer = Layer::new("zones");
        let geometries = [
            Geometry::polygon(ring(2.3, 5.1, 9.7, 290.4), vec![]),
            Geometry::polygon(ring(10., 10., 35., 200.), vec![ring(15., 50., 30., 120.)]),
            Geometry::multi_polygon(vec![(ring(0., 0., 4.5, 3.5), vec![]), (ring(20.2, 250., 39.9, 299.9), vec![])]),
        ];
        for (i, g) in geometries.into_iter().enumerate() {
            layer.features.push(Feature { fid: i as u64, geometry: Some(g), attributes: vec![] });
        }
        let stats: Vec<Statistic> =
            ["count", "mean", "stddev", "median", "quantile_90", "mode", "variety"].iter().map(|s| Statistic::parse(s).unwrap()).collect();
        let fids = [0, 1, 2];
        for fractional in [false, true] {
            let mut raster = LazyRaster::from(ds.clone());
            let index = build(&layer, &GridKey::of(&raster), false, fractional).unwrap();
            let plan = Plan::new(&stats, fractional, &fids);
            let bands = || vec![BandSpec { band: 1, nodata: None }, BandSpec { band: 2, nodata: None }];
            let whole = reduce_source(&index, &mut raster, &bands(), None, &plan, rows, 4).unwrap();
            assert!(whole[0][0].iter().all(|v| !v.is_null()));
            for strip in [1, 7, 64] {
                assert_eq!(reduce_source(&index, &mut raster, &bands(), None, &plan, strip, 1).unwrap(), whole, "strip {strip}");
            }
        }
    }
}

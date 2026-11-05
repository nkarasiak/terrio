//! Moving-window operations computed a row at a time, fast, with the windows that need care
//! (nodata, NaN) recomputed one pixel at a time.
//!
//! A window operation runs a fast path over whole rows: the same arithmetic, in the same order for
//! each pixel, vectorized across pixels. The windows holding a value the fast path cannot handle
//! are found from each row's runs of such values ([`dirty_runs`], once per row) and recomputed
//! with the operation's per-pixel code ([`dirty_windows`]).

/// The runs `start..end` of the values of `row` that `dirty` flags.
pub(crate) fn dirty_runs<T: Copy>(row: &[T], dirty: impl Fn(T) -> bool) -> Vec<(usize, usize)> {
    const CHUNK: usize = 64;
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (c, chunk) in row.chunks(CHUNK).enumerate() {
        // Most chunks have no flagged value: a test without branches, which vectorizes, skips them.
        if !chunk.iter().fold(false, |a, v| a | dirty(*v)) {
            continue;
        }
        for (j, v) in chunk.iter().enumerate() {
            if dirty(*v) {
                let j = c * CHUNK + j;
                match runs.last_mut() {
                    Some(run) if run.1 == j => run.1 = j + 1,
                    _ => runs.push((j, j + 1)),
                }
            }
        }
    }
    runs
}

/// The columns `start..end` (sorted, disjoint, within `0..width`) whose window, `rx` columns
/// on each side, meets one of `runs`: the runs of the window's rows.
pub(crate) fn dirty_windows<'a>(runs: impl IntoIterator<Item = &'a [(usize, usize)]>, rx: usize, width: usize) -> Vec<(usize, usize)> {
    let mut spans: Vec<(usize, usize)> = runs.into_iter().flatten().map(|&(s, e)| (s.saturating_sub(rx), (e + rx).min(width))).collect();
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (s, e) in spans {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_and_the_windows_they_reach() {
        let row: Vec<f32> = (0..200).map(|i| if i == 3 || i == 4 || i == 70 || i == 199 { f32::NAN } else { i as f32 }).collect();
        let runs = dirty_runs(&row, f32::is_nan);
        assert_eq!(runs, vec![(3, 5), (70, 71), (199, 200)]);
        let other = [(5, 6), (100, 101)];
        assert_eq!(dirty_windows([&runs[..], &other[..]], 1, 200), vec![(2, 7), (69, 72), (99, 102), (198, 200)]);
        assert_eq!(dirty_windows([&[(0, 1)][..]], 2, 200), vec![(0, 3)]);
        assert!(dirty_windows([&[][..]], 1, 10).is_empty());
    }
}

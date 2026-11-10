//! Open a Zarr array from objects the caller fetches, read a window as f64.
use std::sync::Arc;
use terrio::sparse::{ObjectRange, SparseObjects};
use wasm_bindgen::prelude::*;

thread_local! { static OBJECTS: Arc<SparseObjects> = Arc::new(SparseObjects::new()); }

/// The keys (whole objects) or ranges asked for by the last run, as `key` or `key@offset+len` / `key@-n`.
#[wasm_bindgen]
pub fn missing() -> Vec<String> {
    OBJECTS.with(|o| {
        o.missing()
            .into_iter()
            .map(|(k, r)| match r {
                ObjectRange::Whole => k,
                ObjectRange::At(a, n) => format!("{k}@{a}+{n}"),
                ObjectRange::Suffix(n) => format!("{k}@-{n}"),
            })
            .collect()
    })
}

/// Hands over an object (or a range, as `missing` named it); `None` when it does not exist.
#[wasm_bindgen]
pub fn insert(part: &str, data: Option<Vec<u8>>) {
    let (key, range) = match part.split_once('@') {
        None => (part, ObjectRange::Whole),
        Some((k, r)) => match r.strip_prefix('-') {
            Some(n) => (k, ObjectRange::Suffix(n.parse().unwrap_or(0))),
            None => {
                let (a, n) = r.split_once('+').unwrap_or(("0", "0"));
                (k, ObjectRange::At(a.parse().unwrap_or(0), n.parse().unwrap_or(0)))
            }
        },
    };
    OBJECTS.with(|o| o.insert(key, range, data));
}

/// Opens the array `key` of the store `name` and reads band `band` of a window: its values as f64,
/// or an error while objects are missing (call `missing`, fetch, `insert`, again).
#[wasm_bindgen]
pub fn read(name: &str, key: &str, band: usize, x: usize, y: usize, w: usize, h: usize) -> Result<Vec<f64>, JsError> {
    let objects = OBJECTS.with(Arc::clone);
    let data = terrio::spike_read(objects, name, key, band, terrio::Window::new(x, y, w, h)).map_err(|e| JsError::new(&e.to_string()))?;
    Ok((0..data.len()).map(|i| data.get_f64(i)).collect())
}

pub mod sun;

use anyhow::Context;
use chrono::{DateTime, SecondsFormat, Utc};
use uom::si::f64::ThermodynamicTemperature;
use uom::si::thermodynamic_temperature::{degree_celsius, kelvin};

/// Kelvin → Celsius (the thermal model carries state in kelvin; reports are in Celsius).
pub fn k_to_c(kelvin_value: f64) -> f64 {
    ThermodynamicTemperature::new::<kelvin>(kelvin_value).get::<degree_celsius>()
}

/// Celsius → Kelvin.
pub fn c_to_k(celsius: f64) -> f64 {
    ThermodynamicTemperature::new::<degree_celsius>(celsius).get::<kelvin>()
}

/// Mean of `sum` over `n` samples (0 when `n == 0`).
pub fn mean(sum: f64, n: usize) -> f64 {
    if n > 0 {
        sum / n as f64
    } else {
        0.0
    }
}

/// Root-mean-square from a sum-of-squared-errors over `n` samples (0 when `n == 0`).
pub fn rmse(sse: f64, n: usize) -> f64 {
    mean(sse, n).sqrt()
}

/// Round `x` to `places` decimal places (half away from zero) — for report/JSON output.
pub fn round_to(x: f64, places: i32) -> f64 {
    let f = 10f64.powi(places);
    (x * f).round() / f
}

/// `t` as RFC 3339 at whole seconds with a `Z` suffix — the API's timestamp format.
pub fn rfc3339_secs(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Sort `items` in place by a descending `f64` key. `f64::total_cmp` gives a genuine total order —
/// a NaN-as-`Equal` comparator would be intransitive (NaN == 1.0, NaN == 2.0, yet 1.0 < 2.0), which
/// `slice::sort_by` detects since Rust 1.81 and PANICS on ("user-provided comparison is not a total
/// order"). NaN keys sort last (total_cmp places -NaN below every number; the descending flip puts
/// +NaN first — so map NaN to -inf explicitly to keep the worst-first lists honest).
pub fn sort_desc_by_key<T>(items: &mut [T], key: impl Fn(&T) -> f64) {
    let k = |x: &T| {
        let v = key(x);
        if v.is_nan() {
            f64::NEG_INFINITY
        } else {
            v
        }
    };
    items.sort_by(|a, b| k(b).total_cmp(&k(a)));
}

/// Replace `path` with `bytes` atomically and durably: write a per-process temp sibling, fsync it,
/// rename it over `path`, then (best effort) fsync the directory. Without the data fsync a power cut
/// can land the rename ahead of the data and leave an empty file. Creates the parent directory;
/// removes the temp file when the write or rename fails.
pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => std::path::Path::new("."),
    };
    std::fs::create_dir_all(dir)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = std::path::PathBuf::from(tmp);
    let replaced = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if replaced.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return replaced;
    }
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Why [`read_json_store`] failed.
#[derive(Debug)]
pub enum StoreError {
    /// The content is bad (not JSON of the store's shape, or not UTF-8): no retry will read it.
    Corrupt(anyhow::Error),
    /// Any other IO error (EIO, EMFILE, EACCES, ...): the file may be fine, so it is left alone.
    Unreadable(anyhow::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Corrupt(e) | StoreError::Unreadable(e) => write!(f, "{e:#}"),
        }
    }
}

/// Read and parse a JSON store; `Ok(None)` when the file does not exist.
pub fn read_json_store<T: serde::de::DeserializeOwned>(
    path: &std::path::Path,
) -> Result<Option<T>, StoreError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            let corrupt = e.kind() == std::io::ErrorKind::InvalidData;
            let e = anyhow::Error::new(e).context("unreadable");
            return Err(if corrupt {
                StoreError::Corrupt(e)
            } else {
                StoreError::Unreadable(e)
            });
        }
    };
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|e| StoreError::Corrupt(anyhow::Error::new(e).context("unparseable")))
}

/// Where a corrupt store is moved aside: stamped, so a later quarantine never lands on an earlier
/// copy.
pub fn quarantine_path(path: &std::path::Path, stamp: DateTime<Utc>) -> std::path::PathBuf {
    let mut aside = path.as_os_str().to_owned();
    aside.push(format!(".corrupt-{}", stamp.format("%Y%m%dT%H%M%S%.6fZ")));
    aside.into()
}

/// Move a corrupt store to [`quarantine_path`]. Refuses, moving nothing, when that name is taken;
/// `Ok(None)` when the store is already gone (nothing left to preserve).
pub fn quarantine_store(
    path: &std::path::Path,
    stamp: DateTime<Utc>,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    let aside = quarantine_path(path, stamp);
    anyhow::ensure!(
        !aside.exists(),
        "could not move it to {}: already exists",
        aside.display()
    );
    match std::fs::rename(path, &aside) {
        Ok(()) => Ok(Some(aside)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            Err(anyhow::Error::new(e).context(format!("could not move it to {}", aside.display())))
        }
    }
}

/// Where a long-lived JSON store stands with its file; kept under the store's write lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreFile {
    /// Read (or absent): the next write may replace it.
    Ready,
    /// Corrupt: moved aside by the next write.
    Corrupt,
    /// Present but unreadable: never written over, and re-read before every write until it reads.
    Unread,
}

/// Open a JSON store: its value and file state. A bad file is logged and starts the value empty;
/// nothing on disk is touched here, so a caller that never writes never moves anything.
pub fn load_json_store<T>(path: &std::path::Path, label: &str) -> (T, StoreFile)
where
    T: serde::de::DeserializeOwned + Default,
{
    match read_json_store(path) {
        Ok(value) => (value.unwrap_or_default(), StoreFile::Ready),
        Err(StoreError::Corrupt(e)) => {
            eprintln!(
                "[{label}] ERROR: store at {} is {e:#} — starting EMPTY in memory; the next write \
                 moves it aside to {}.corrupt-<time> first",
                path.display(),
                path.display()
            );
            (T::default(), StoreFile::Corrupt)
        }
        Err(StoreError::Unreadable(e)) => {
            eprintln!(
                "[{label}] ERROR: store at {} is {e:#} — starting EMPTY in memory; nothing is \
                 written over it until a later read succeeds",
                path.display()
            );
            (T::default(), StoreFile::Unread)
        }
    }
}

/// Make a store's file safe to replace, under its write lock: re-read an [`StoreFile::Unread`]
/// file, move a [`StoreFile::Corrupt`] one aside. `Ok(Some(value))` is the content of a file that
/// has read at last, for the caller to adopt in place of what it built on the empty start. An
/// error leaves the file as it is, so the caller must not write and the next write retries.
pub fn prepare_store_write<T: serde::de::DeserializeOwned>(
    path: &std::path::Path,
    file: &mut StoreFile,
    label: &str,
) -> anyhow::Result<Option<T>> {
    if *file == StoreFile::Unread {
        match read_json_store(path) {
            Ok(Some(value)) => {
                *file = StoreFile::Ready;
                return Ok(Some(value));
            }
            Ok(None) => *file = StoreFile::Ready,
            Err(StoreError::Unreadable(e)) => {
                return Err(e).context(format!("the {label} store is left as it is"));
            }
            Err(StoreError::Corrupt(e)) => {
                eprintln!("[{label}] store at {} is {e:#}", path.display());
                *file = StoreFile::Corrupt;
            }
        }
    }
    if *file == StoreFile::Corrupt {
        let moved = quarantine_store(path, Utc::now())
            .with_context(|| format!("the {label} store is corrupt"))?;
        if let Some(aside) = moved {
            eprintln!(
                "[{label}] moved the corrupt store to {} before writing fresh state",
                aside.display()
            );
        }
        *file = StoreFile::Ready;
    }
    Ok(None)
}

/// Serializes the tests that mutate the process environment (`MPC_*_STORE` path overrides).
///
/// `cargo test` runs them on parallel threads in ONE process, and glibc's `setenv` can realloc
/// `environ` out from under a concurrent `getenv` — a genuine data race (which is why `set_var` is
/// `unsafe` from the 2024 edition), showing up as rare crashes or one test reading another's path.
/// Acquire it for the whole body of any test that touches the environment.
#[cfg(test)]
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    #[test]
    fn write_atomic_replaces_creates_parents_and_leaves_no_temp() {
        let dir = std::env::temp_dir().join(format!("mpc-write-atomic-{}", std::process::id()));
        let path = dir.join("nested").join("store.json");
        super::write_atomic(&path, b"one").unwrap();
        super::write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("store.json")]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn conversions_round_trip() {
        assert!((super::c_to_k(0.0) - 273.15).abs() < 1e-9);
        assert!((super::k_to_c(super::c_to_k(21.5)) - 21.5).abs() < 1e-9);
    }

    #[test]
    fn sort_desc_orders_finite_high_to_low() {
        let mut v = vec![1.0, 3.0, 2.0, 0.5];
        super::sort_desc_by_key(&mut v, |x| *x);
        assert_eq!(v, vec![3.0, 2.0, 1.0, 0.5]);
    }

    #[test]
    fn sort_desc_is_nan_safe() {
        // A non-comparable key must not panic (the call sites only ever pass finite stats, but the
        // comparator stays total either way).
        let mut v = vec![1.0, f64::NAN, 2.0];
        super::sort_desc_by_key(&mut v, |x| *x);
        assert_eq!(v.len(), 3);
    }
}

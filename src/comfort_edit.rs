//! Edit one room's comfort target in the LIVE `config.json5` without disturbing anything else.
//!
//! The file is the single source of truth and is hand-commented, so it is never re-serialised:
//! exactly one number (the `target:` of the room's `heat` or `cool` block) is replaced in the text.
//! The candidate is written beside the original, fully validated with [`ControlConfig::load`], and
//! only then renamed over it; any failure leaves the original byte-identical. One global lock
//! serialises writers.

use crate::optimize::config::ControlConfig;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The config file the server loads at boot and edits here: `MPC_CONFIG_PATH`, default
/// `config.json5` in the working directory. Read once, so every loader and the editor agree.
pub fn config_path() -> &'static str {
    static PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        std::env::var("MPC_CONFIG_PATH")
            .ok()
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| "config.json5".to_string())
    })
}

/// Allowed step of a target (°C).
pub const STEP_C: f64 = 0.5;
/// Heat target clamp (°C).
pub const HEAT_RANGE: (f64, f64) = (15.0, 26.0);
/// Cool target clamp (°C).
pub const COOL_RANGE: (f64, f64) = (20.0, 30.0);

/// Which target of the room is edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Heat,
    Cool,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Heat => "heat",
            Kind::Cool => "cool",
        }
    }

    fn range(self) -> (f64, f64) {
        match self {
            Kind::Heat => HEAT_RANGE,
            Kind::Cool => COOL_RANGE,
        }
    }
}

/// Why an edit was refused.
#[derive(Debug)]
pub enum EditError {
    /// The request or the resulting config is not acceptable (HTTP 422). The file is untouched.
    Invalid(String),
    /// The file on disk is not the one the live config came from (HTTP 409). The file is untouched.
    Conflict,
    /// The file could not be read or written (HTTP 500). The original is untouched.
    Io(anyhow::Error),
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::Invalid(m) => f.write_str(m),
            EditError::Conflict => f.write_str(
                "config.json5 changed on disk since it was loaded; restart the brain first",
            ),
            EditError::Io(e) => write!(f, "{e:#}"),
        }
    }
}

/// Serialises every edit (read, validate, rename).
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Check the value against the step and range rules.
pub fn validate_value(kind: Kind, value: f64) -> Result<(), EditError> {
    let name = format!("{}_target", kind.name());
    let (lo, hi) = kind.range();
    if !value.is_finite() {
        return Err(EditError::Invalid(format!(
            "{name} must be a finite number"
        )));
    }
    if value < lo || value > hi {
        return Err(EditError::Invalid(format!(
            "{name} must be within {lo:.1}-{hi:.1}"
        )));
    }
    let steps = value / STEP_C;
    if (steps - steps.round()).abs() > 1e-9 {
        return Err(EditError::Invalid(format!(
            "{name} must be a multiple of {STEP_C}"
        )));
    }
    Ok(())
}

/// Text of `line` with a trailing `//` comment removed (comment markers inside this file's quoted
/// strings do not occur in the comfort block).
fn code_part(line: &str) -> &str {
    line.find("//").map_or(line, |i| &line[..i])
}

/// Line index range `[start, end)` of the top-level `comfort: { ... }` block.
fn comfort_block(lines: &[&str]) -> Option<(usize, usize)> {
    let start = lines.iter().position(|l| {
        let t = code_part(l).trim_start();
        t.starts_with("comfort:") && t.contains('{')
    })?;
    let mut depth = 0i32;
    for (i, l) in lines.iter().enumerate().skip(start) {
        for c in code_part(l).chars() {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
        }
        if depth <= 0 {
            return Some((start, i + 1));
        }
    }
    None
}

/// `text` with the number after `<kind>: { target:` on the room's line replaced by `value`.
/// Everything else, including line endings and comments, is kept byte-for-byte.
pub fn replace_target(text: &str, zone: &str, kind: Kind, value: f64) -> Result<String, EditError> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let (start, end) = comfort_block(&lines).ok_or_else(|| {
        EditError::Invalid("config.json5 has no `comfort: {` block to edit".to_string())
    })?;
    let prefix = format!("{zone}:");
    let hits: Vec<usize> = (start..end)
        .filter(|&i| code_part(lines[i]).trim_start().starts_with(&prefix))
        .collect();
    let [idx] = hits[..] else {
        return Err(EditError::Invalid(format!(
            "expected exactly one `{zone}:` line in the comfort block, found {}",
            hits.len()
        )));
    };
    let line = lines[idx];
    let code_len = code_part(line).len();
    let pattern = format!("{}: {{ target:", kind.name());
    let matches: Vec<usize> = line[..code_len]
        .match_indices(&pattern)
        .map(|m| m.0)
        .collect();
    let [at] = matches[..] else {
        return Err(EditError::Invalid(
            if matches.is_empty() && kind == Kind::Cool {
                format!("room {zone} has no cool block")
            } else {
                format!(
                    "cannot edit {zone}: the line must contain exactly one `{pattern} <number>` \
                 (found {})",
                    matches.len()
                )
            },
        ));
    };
    let num_start = at + pattern.len();
    let rest = &line[num_start..code_len];
    let lead = rest.len() - rest.trim_start().len();
    let digits = rest[lead..]
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+')))
        .unwrap_or(rest.len() - lead);
    if digits == 0 || rest[lead..lead + digits].parse::<f64>().is_err() {
        return Err(EditError::Invalid(format!(
            "cannot edit {zone}: no plain number after `{pattern}`"
        )));
    }
    let (a, b) = (num_start + lead, num_start + lead + digits);
    let mut out = String::with_capacity(text.len() + 4);
    for l in &lines[..idx] {
        out.push_str(l);
    }
    out.push_str(&line[..a]);
    out.push_str(&format!("{value:.1}"));
    out.push_str(&line[b..]);
    for l in &lines[idx + 1..] {
        out.push_str(l);
    }
    Ok(out)
}

/// A temporary path beside `path`, unique to this writer so no other process staging the same
/// file (e.g. the deploy script) can collide with it.
fn tmp_path(path: &Path) -> PathBuf {
    let name = path.file_name().map_or_else(
        || "config.json5".into(),
        |n| n.to_string_lossy().into_owned(),
    );
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    path.with_file_name(format!(".{name}.{}.{nanos}.tmp", std::process::id()))
}

/// Set `zone`'s heat or cool target to `value` in the file at `path` (see [`set_targets`]).
#[cfg(test)]
pub fn set_target(
    path: &Path,
    zone: &str,
    kind: Kind,
    value: f64,
    commit: impl FnOnce(&ControlConfig, &str),
) -> Result<ControlConfig, EditError> {
    set_targets(path, zone, &[(kind, value)], |_| true, |_| Ok(()), commit)
}

/// Apply every `(kind, value)` edit to `zone`'s line in one write.
///
/// `is_live` is asked, under the write lock, whether the text read from disk is the file the live
/// config came from; if not the edit is refused with [`EditError::Conflict`], so a hand edit or a
/// half-done deploy is never adopted through the comfort block. The edits are then validated (step,
/// range, the whole-file [`ControlConfig::load`], then the caller's `check` on the loaded candidate)
/// before the original is replaced. On success `commit` runs with the freshly loaded config and the
/// written text while the write lock is still held, so the live config is swapped in the same order
/// as the file.
pub fn set_targets(
    path: &Path,
    zone: &str,
    edits: &[(Kind, f64)],
    is_live: impl FnOnce(&str) -> bool,
    check: impl FnOnce(&ControlConfig) -> Result<(), String>,
    commit: impl FnOnce(&ControlConfig, &str),
) -> Result<ControlConfig, EditError> {
    set_targets_with(
        path,
        zone,
        edits,
        is_live,
        check,
        |a, b| std::fs::rename(a, b),
        commit,
    )
}

/// [`set_targets`] with the final rename injectable, so the failure path is testable.
fn set_targets_with(
    path: &Path,
    zone: &str,
    edits: &[(Kind, f64)],
    is_live: impl FnOnce(&str) -> bool,
    check: impl FnOnce(&ControlConfig) -> Result<(), String>,
    rename: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
    commit: impl FnOnce(&ControlConfig, &str),
) -> Result<ControlConfig, EditError> {
    for &(kind, value) in edits {
        validate_value(kind, value)?;
    }
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut edited = std::fs::read_to_string(path)
        .map_err(|e| EditError::Io(anyhow::Error::new(e).context("reading config.json5")))?;
    if !is_live(&edited) {
        return Err(EditError::Conflict);
    }
    for &(kind, value) in edits {
        edited = replace_target(&edited, zone, kind, value)?;
    }
    let tmp = tmp_path(path);
    let write_synced = || -> std::io::Result<()> {
        use std::io::Write;
        let permissions = std::fs::metadata(path)?.permissions();
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.set_permissions(permissions)?;
        f.write_all(edited.as_bytes())?;
        f.sync_all()
    };
    if let Err(e) = write_synced() {
        let _ = std::fs::remove_file(&tmp);
        return Err(EditError::Io(
            anyhow::Error::new(e).context("writing the temporary config"),
        ));
    }
    let config = match ControlConfig::load(&tmp) {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(EditError::Invalid(format!("{e:#}")));
        }
    };
    if let Err(m) = check(&config) {
        let _ = std::fs::remove_file(&tmp);
        return Err(EditError::Invalid(m));
    }
    if let Err(e) = rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(EditError::Io(
            anyhow::Error::new(e).context("replacing config.json5"),
        ));
    }
    // Best effort: make the rename itself durable.
    if let Some(dir) = path.parent() {
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    commit(&config, &edited);
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json5");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("config.json5"),
            &path,
        )
        .unwrap();
        (dir, path)
    }

    fn assert_no_temp_left(path: &Path) {
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [path.file_name().unwrap()]);
    }

    fn set(path: &Path, zone: &str, kind: Kind, v: f64) -> Result<ControlConfig, EditError> {
        set_target(path, zone, kind, v, |_, _| {})
    }

    #[test]
    fn heat_edit_changes_exactly_one_line() {
        let (_d, path) = fixture();
        let before = std::fs::read_to_string(&path).unwrap();
        let mut committed = None;
        let cfg = set_target(&path, "ground_bathroom", Kind::Heat, 21.5, |c, text| {
            committed = Some((
                c.heating.comfort.rooms["ground_bathroom"].heat.target,
                text.len(),
            ));
        })
        .unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            cfg.heating.comfort.rooms["ground_bathroom"].heat.target,
            21.5
        );
        assert_eq!(committed.unwrap().0, 21.5);
        let diff: Vec<_> = before
            .lines()
            .zip(after.lines())
            .filter(|(a, b)| a != b)
            .collect();
        assert_eq!(before.lines().count(), after.lines().count());
        assert_eq!(diff.len(), 1);
        assert_eq!(
            diff[0].1,
            diff[0]
                .0
                .replacen("heat: { target: 22.0", "heat: { target: 21.5", 1)
        );
        assert_no_temp_left(&path);
        // comments untouched
        assert!(after.contains("// --- top tier (priority 1)"));
    }

    #[test]
    fn heat_and_cool_apply_in_one_write() {
        let (_d, path) = fixture();
        let cfg = set_targets(
            &path,
            "livingroom",
            &[(Kind::Heat, 21.0), (Kind::Cool, 26.5)],
            |_| true,
            |_| Ok(()),
            |_, _| {},
        )
        .unwrap();
        let room = &cfg.heating.comfort.rooms["livingroom"];
        assert_eq!(
            (room.heat.target, room.cool.as_ref().unwrap().target),
            (21.0, 26.5)
        );
    }

    #[test]
    fn cool_edit_changes_only_cool_number() {
        let (_d, path) = fixture();
        let cfg = set(&path, "livingroom", Kind::Cool, 26.0).unwrap();
        assert_eq!(
            cfg.heating.comfort.rooms["livingroom"]
                .cool
                .as_ref()
                .unwrap()
                .target,
            26.0
        );
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains(
            "livingroom: { heat: { target: 22.0, below: 1.0, above: 2.0 }, cool: { target: 26.0, delta: 1.0 }, priority: 1.0 },"
        ));
    }

    #[test]
    fn every_refusal_leaves_the_file_byte_identical() {
        let (_d, path) = fixture();
        let before = std::fs::read(&path).unwrap();
        let cases: [(&str, Kind, f64); 6] = [
            ("ground_bathroom", Kind::Heat, 22.3),
            ("ground_bathroom", Kind::Heat, 14.0),
            ("ground_bathroom", Kind::Heat, 26.5),
            ("ground_bathroom", Kind::Heat, f64::NAN),
            ("storage", Kind::Cool, 25.0),
            ("nonexistent", Kind::Heat, 21.0),
        ];
        for (zone, kind, v) in cases {
            let err = set(&path, zone, kind, v).unwrap_err();
            assert!(matches!(err, EditError::Invalid(_)), "{zone} {v}: {err}");
            assert_eq!(std::fs::read(&path).unwrap(), before, "{zone} {v}");
            assert_no_temp_left(&path);
        }
    }

    #[test]
    fn a_config_that_fails_validation_is_never_swapped_in() {
        let (_d, path) = fixture();
        // The edit itself is legal, but the file as a whole no longer loads.
        let broken = std::fs::read_to_string(&path).unwrap().replacen(
            "pull_eur_per_kh: 0.04",
            "pull_eur_per_kh: 0.04, bogus_key: 1",
            1,
        );
        std::fs::write(&path, &broken).unwrap();
        let err = set_target(&path, "ground_bathroom", Kind::Heat, 21.5, |_, _| {
            panic!("must not commit")
        })
        .unwrap_err();
        assert!(
            matches!(err, EditError::Invalid(ref m) if m.contains("bogus_key")),
            "{err}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        assert_no_temp_left(&path);
    }

    #[test]
    fn a_failing_check_leaves_the_file_untouched() {
        let (_d, path) = fixture();
        let before = std::fs::read(&path).unwrap();
        let err = set_targets_with(
            &path,
            "ground_bathroom",
            &[(Kind::Heat, 21.5)],
            |_| true,
            |_| Err("nope".to_string()),
            |a, b| std::fs::rename(a, b),
            |_, _| panic!("must not commit"),
        )
        .unwrap_err();
        assert!(matches!(err, EditError::Invalid(ref m) if m == "nope"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_no_temp_left(&path);
    }

    #[test]
    fn a_file_changed_since_load_is_refused() {
        let (_d, path) = fixture();
        let before = std::fs::read_to_string(&path).unwrap();
        let mut seen = None;
        let err = set_targets(
            &path,
            "ground_bathroom",
            &[(Kind::Heat, 21.5)],
            |text| {
                seen = Some(text.to_string());
                false
            },
            |_| Ok(()),
            |_, _| panic!("must not commit"),
        )
        .unwrap_err();
        assert!(matches!(err, EditError::Conflict), "{err}");
        assert_eq!(seen.as_deref(), Some(before.as_str()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert_no_temp_left(&path);
    }

    #[cfg(unix)]
    #[test]
    fn the_original_permissions_are_kept() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, path) = fixture();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        set(&path, "ground_bathroom", Kind::Heat, 21.5).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
        assert_no_temp_left(&path);
    }

    #[test]
    fn the_temp_file_is_a_hidden_sibling_of_the_original() {
        let path = Path::new("/srv/conf/config.json5");
        let tmp = tmp_path(path);
        assert_eq!(tmp.parent(), path.parent());
        let name = tmp.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with(".config.json5.") && name.ends_with(".tmp"),
            "{name}"
        );
    }

    #[test]
    fn duplicate_or_missing_line_is_refused() {
        let text = "{\n comfort: {\n rooms: {\n a: { heat: { target: 20.0, delta: 1 } },\n a: { heat: { target: 20.0, delta: 1 } },\n } }\n}\n";
        assert!(matches!(
            replace_target(text, "a", Kind::Heat, 21.0),
            Err(EditError::Invalid(_))
        ));
        assert!(matches!(
            replace_target(text, "b", Kind::Heat, 21.0),
            Err(EditError::Invalid(_))
        ));
        // a room named only in a comment or outside the block is not a hit
        let text = "{\n a: { heat: { target: 1.0 } },\n comfort: {\n // b: { heat: { target: 20.0 } },\n }\n}\n";
        assert!(replace_target(text, "a", Kind::Heat, 21.0).is_err());
        assert!(replace_target(text, "b", Kind::Heat, 21.0).is_err());
    }

    #[test]
    fn crlf_and_trailing_comment_survive() {
        let text = "{\r\n comfort: {\r\n a: { heat: { target: 20, delta: 1 }, cool: { target: 25.0 } }, // keep me\r\n }\r\n}\r\n";
        let out = replace_target(text, "a", Kind::Heat, 21.5).unwrap();
        assert_eq!(out, text.replacen("target: 20,", "target: 21.5,", 1));
    }

    #[cfg(unix)]
    #[test]
    fn failed_write_leaves_the_original_intact() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, path) = fixture();
        let before = std::fs::read(&path).unwrap();
        // Read-only directory: the temporary file cannot be created.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let res = set(&path, "ground_bathroom", Kind::Heat, 21.5);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        if let Err(e) = res {
            assert!(matches!(e, EditError::Io(_)), "{e}");
        } else {
            // running as root ignores directory permissions; nothing to prove then
            return;
        }
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn failed_rename_leaves_the_original_intact() {
        let (_d, path) = fixture();
        let before = std::fs::read(&path).unwrap();
        let err = set_targets_with(
            &path,
            "ground_bathroom",
            &[(Kind::Heat, 21.5)],
            |_| true,
            |_| Ok(()),
            |_, _| Err(std::io::Error::other("injected")),
            |_, _| panic!("must not commit"),
        )
        .unwrap_err();
        assert!(matches!(err, EditError::Io(_)));
        assert_no_temp_left(&path);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

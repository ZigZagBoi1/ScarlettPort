//! Save slots (`SCARLETT_SAVE_SLOT=<name>`, off by default): each slot is a
//! whole game under `<data>/<TITLEID>/slots/<name>/storage`; a new name
//! starts fresh, an existing one continues. Unset, the title keeps its
//! usual `<data>/<TITLEID>/storage`.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// The slot named by `SCARLETT_SAVE_SLOT`, validated; `None` when unset or empty.
pub fn slot_from_env() -> Result<Option<String>> {
    let Some(name) = std::env::var("SCARLETT_SAVE_SLOT").ok().filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    validate(&name)?;
    Ok(Some(name))
}

/// Names are 1 to 64 of letters, digits, `-`, `_` and `.` (not leading), so
/// they can never hold a path separator or climb out.
pub fn validate(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if !ok {
        bail!(
            "invalid save slot name {name:?}: use 1-64 letters, digits, '-', '_' or '.', \
             not starting with '.'"
        );
    }
    Ok(())
}

/// The storage folder of `slot` under a title folder (relative to the
/// title's data folder), without creating anything.
pub fn storage(title: &Path, slot: Option<&str>) -> Result<PathBuf> {
    Ok(match slot {
        None => title.join("storage"),
        Some(slot) => {
            validate(slot)?;
            title.join("slots").join(slot).join("storage")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_reject_separators_and_escapes() {
        let long = "n".repeat(65);
        for bad in ["", "a/b", "a\\b", "..", ".hidden", "a:b", "x y", long.as_str()] {
            assert!(validate(bad).is_err(), "{bad:?}");
        }
        for good in ["default", "run-2", "A_b.1"] {
            validate(good).unwrap();
        }
    }

    #[test]
    fn slot_paths() {
        let p = Path::new("t");
        assert_eq!(storage(p, None).unwrap(), p.join("storage"));
        assert_eq!(storage(p, Some("b")).unwrap(), p.join("slots/b/storage"));
        assert!(storage(p, Some("../x")).is_err());
    }
}

//! Environment switches: every switch is a `SCARLETT_*` variable.

use std::env::VarError;
use std::ffi::OsString;

const LEGACY_PREFIX: &str = "VICTORY_"; // rename:keep

fn legacy(name: &str) -> Option<String> {
    name.strip_prefix("SCARLETT_").map(|rest| format!("{LEGACY_PREFIX}{rest}"))
}

/// `std::env::var`.
pub fn var(name: &str) -> Result<String, VarError> {
    match std::env::var(name) {
        Err(VarError::NotPresent) => match legacy(name) {
            Some(old) => std::env::var(old),
            None => Err(VarError::NotPresent),
        },
        other => other,
    }
}

/// `std::env::var_os`.
pub fn var_os(name: &str) -> Option<OsString> {
    std::env::var_os(name).or_else(|| legacy(name).and_then(std::env::var_os))
}

/// The variable's value when it is set (the name `env(name)` of the spec).
pub fn env(name: &str) -> Option<String> {
    var(name).ok()
}

/// At startup, before any thread: fills every unset `SCARLETT_*` variable
/// from its older spelling, so code reading `std::env` directly sees it too.
pub fn alias_scarlett() {
    let pairs: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter_map(|(k, v)| {
            let rest = k.to_str()?.strip_prefix(LEGACY_PREFIX)?;
            let new = format!("SCARLETT_{rest}");
            std::env::var_os(&new).is_none().then(|| (OsString::from(new), v))
        })
        .collect();
    for (k, v) in pairs {
        // SAFETY: called first thing in `main`, before other threads exist.
        unsafe { std::env::set_var(k, v) };
    }
}
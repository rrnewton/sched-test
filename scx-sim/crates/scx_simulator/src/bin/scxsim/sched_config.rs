//! Scheduler configuration loaded from a TOML file via `scxsim run --config`.
//!
//! # Why typed sub-tables
//!
//! `DynamicScheduler::get_symbol::<T>()` requires the caller to declare the
//! symbol type — there is no introspection that can derive `T` from the .so's
//! ELF/DWARF. So the caller (this module) must know whether a given global is
//! a `bool`, `u8`, `u32`, or `u64`. The TOML file therefore segregates globals
//! by primitive type into named sub-tables.
//!
//! Example:
//!
//! ```toml
//! [scheduler.bool_globals]
//! enable_cpu_bw = true
//!
//! [scheduler.u32_globals]
//! some_threshold = 1024
//!
//! [scheduler.u64_globals]
//! period_ns = 1_000_000_000
//! ```
//!
//! # FFI safety
//!
//! `apply_to_scheduler` writes through the .so's `const volatile` BPF globals
//! using `std::ptr::write_volatile`. The same pattern is used elsewhere in the
//! test/runtime (`tests/h6_matrix.rs`, `tests/bug1_canonical_repro.rs`). The
//! caller asserts that the type declared in the TOML sub-table matches the
//! actual symbol type in the .so; a mismatch is undefined behavior.

use std::path::Path;

use scx_simulator::DynamicScheduler;
use serde::Deserialize;

/// Top-level scheduler config file shape.
///
/// The single `[scheduler]` table groups all per-symbol sub-tables. Other
/// top-level tables (e.g. `[engine]`) are reserved for future use and are
/// rejected as unknown fields today so config-file typos are loud.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerConfigFile {
    #[serde(default)]
    pub scheduler: SchedulerSection,
}

/// `[scheduler.*]` sub-tables: typed BPF-global sets.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerSection {
    /// `[scheduler.bool_globals]` — `const volatile bool` symbols.
    #[serde(default)]
    pub bool_globals: std::collections::BTreeMap<String, bool>,

    /// `[scheduler.u8_globals]` — `const volatile u8` symbols.
    #[serde(default)]
    pub u8_globals: std::collections::BTreeMap<String, u8>,

    /// `[scheduler.u32_globals]` — `const volatile u32` symbols.
    #[serde(default)]
    pub u32_globals: std::collections::BTreeMap<String, u32>,

    /// `[scheduler.u64_globals]` — `const volatile u64` symbols.
    #[serde(default)]
    pub u64_globals: std::collections::BTreeMap<String, u64>,
}

/// Load a config file from disk, returning a parsed `SchedulerConfigFile`.
pub fn load_config(path: &Path) -> Result<SchedulerConfigFile, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read config {}: {e}", path.display()))?;
    toml::from_str(&contents).map_err(|e| format!("failed to parse config {}: {e}", path.display()))
}

/// Walk the typed sub-tables and write each value through to the loaded
/// scheduler `.so`'s corresponding `const volatile` global.
///
/// Symbols are looked up by literal name (a NUL terminator is appended for the
/// libloading lookup). A missing symbol produces an `Err` so config typos are
/// loud rather than silently no-ops.
pub fn apply_to_scheduler(
    config: &SchedulerConfigFile,
    sched: &DynamicScheduler,
) -> Result<(), String> {
    for (name, value) in &config.scheduler.bool_globals {
        // SAFETY: caller declared this symbol is a `bool` via the
        // `[scheduler.bool_globals]` sub-table; a type mismatch in the .so is
        // a config error.
        unsafe {
            write_global::<bool>(sched, name, *value)?;
        }
    }
    for (name, value) in &config.scheduler.u8_globals {
        // SAFETY: caller declared `u8` via `[scheduler.u8_globals]`.
        unsafe {
            write_global::<u8>(sched, name, *value)?;
        }
    }
    for (name, value) in &config.scheduler.u32_globals {
        // SAFETY: caller declared `u32` via `[scheduler.u32_globals]`.
        unsafe {
            write_global::<u32>(sched, name, *value)?;
        }
    }
    for (name, value) in &config.scheduler.u64_globals {
        // SAFETY: caller declared `u64` via `[scheduler.u64_globals]`.
        unsafe {
            write_global::<u64>(sched, name, *value)?;
        }
    }
    Ok(())
}

/// Common helper: look up a symbol by name (NUL-appended) and write through
/// it with `ptr::write_volatile`.
///
/// # Safety
/// Caller must ensure `T` matches the actual ELF symbol type in the loaded
/// scheduler `.so`.
unsafe fn write_global<T>(sched: &DynamicScheduler, name: &str, value: T) -> Result<(), String> {
    let mut nul_name = String::with_capacity(name.len() + 1);
    nul_name.push_str(name);
    nul_name.push('\0');
    let sym: libloading::Symbol<'_, *mut T> = sched
        .get_symbol(nul_name.as_bytes())
        .ok_or_else(|| format!("scheduler symbol {name:?} not found in .so"))?;
    std::ptr::write_volatile(*sym, value);
    Ok(())
}

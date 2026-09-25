//! Negative control for `embed_harness`: this package has no build script, so
//! its test binary is linked without `scxsim_build::emit_host_link_args()` --
//! the call every package whose binaries load a scheduler must make itself,
//! because cargo does not pass link arguments on to dependents. None of
//! `HOST_EXPORTS` is then in the binary's dynamic symbol table, and a scheduler
//! `.so` dlopen'd into it would either fail on an undefined symbol or load and
//! run with its own stubs (or NULL) bound in place of the simulator's
//! definitions, green and wrong. The loader must refuse before `dlopen`, with
//! one error naming every missing export and the fix.

use scx_simulator::prelude::*;
use scxsim_build::HOST_EXPORTS;

#[test]
fn a_binary_without_host_link_args_is_refused_before_dlopen() {
    // No such file: the refusal has to come from the probe, which runs before
    // the `.so` is opened, and not from a failed `dlopen`.
    let so = "/nonexistent/libscx_simple.so";
    let def = SchedulerDefinition::new("simple");
    let err = match DynamicScheduler::try_load_with_definition(so, &def, 1) {
        Ok(_) => panic!("loaded a scheduler into a binary that exports no host symbols"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains("scxsim_build::emit_host_link_args()"),
        "{message}"
    );
    assert_eq!(
        err,
        LoadError::HostSymbolsNotExported {
            path: so.to_owned(),
            missing: HOST_EXPORTS.to_vec(),
        },
        "{message}"
    );
}

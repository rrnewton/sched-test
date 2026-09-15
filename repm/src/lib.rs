//! ReproMagic — capture and reproduce scheduling behavior.
//!
//! # Why this crate has a library target
//!
//! repm was binary-only, and everything here was reachable only from `main` or
//! from tests. That made `dead_code` fire on the parsers, the synthesis scoring
//! and their supporting types — roughly a dozen findings — because in a
//! bin-only crate the lint asks "is this reachable from `main`?", and for a
//! crate whose core is trace parsing with a thin CLI over it that is the wrong
//! question. The code is exercised, and `pub` on it was already intended as an
//! API surface: `main.rs` declared `pub mod compare` and `pub mod score` long
//! before this file existed.
//!
//! Declaring the library makes that surface real, so the lint is answering the
//! question it should: is this part of the crate's API? It also lets
//! `cargo test --doc` run at all — with no lib target it errors out with "no
//! library targets found", which is why repm's CI has no doc-test step.
//!
//! This is NOT a way to silence the dead-code findings. Anything genuinely
//! unreachable under either model was removed rather than re-labelled; see the
//! commit that introduced this file.

pub mod commands;
pub mod compare;
pub mod config;
pub mod score;
pub mod synthesis;
pub mod templates;
pub mod trace;
pub mod workspace;

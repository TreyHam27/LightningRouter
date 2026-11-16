//! Whole-system mode (stage B). Only `pub mod` lines live here.

pub mod fakedns;
pub mod mode;
pub mod setup;
pub mod stack;
#[allow(dead_code)] // run only by examples/system_spike.rs, which includes it by path
pub mod stub;

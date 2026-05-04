//! Linear tracker module: webhook + polling adapters and normalized issue
//! model. Real adapter implementation lands in tasks 3.x.
//!
//! Spec refs: requirements.md Req 3.x, design.md File Structure Plan
//! `tracker/`.

pub mod linear;
pub mod model;
pub mod pre_admission;
pub mod refresh;
pub mod webhook;

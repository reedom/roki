//! In-memory observability primitives. Ring buffer + (future) hooks.

pub mod ring;

pub use ring::EventRing;

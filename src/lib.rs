//! nixdeploy: getting a prebuilt Nix closure onto a machine that did not build it, and
//! knowing afterwards whether it got there.
//!
//! Both halves of that sentence live in this one crate, on purpose:
//!
//!   * [`publish`] renders, signs and writes the manifest naming what every machine should
//!     be running. It builds nothing and uploads nothing -- see its module doc.
//!   * [`receive`] runs on each managed machine, verifies that manifest, sizes the change
//!     against its OWN store, and either activates, refuses, or routes the refusal to a
//!     reimage.
//!
//! They share [`manifest`], which is the whole reason they are one crate rather than two: the
//! type that writes a manifest is the type that reads one, and the bytes the publisher signs
//! are produced by the function the receiver verifies against. Two crates would have to keep
//! the schema version, the field names and the canonical byte form in sync by hand across a
//! boundary where a mismatch is silent on both sides.
//!
//! `main.rs` is a thin dispatcher over these modules. Everything worth testing is here, so
//! integration tests under `tests/` can drive a whole publish-then-receive round trip in one
//! process instead of shelling out to a built binary and scraping stdout.

pub mod activate;
pub mod atomicfile;
pub mod delta;
pub mod manifest;
pub mod metrics;
pub mod outcome;
pub mod promote;
pub mod publish;
pub mod receive;
pub mod release;

pub use outcome::{Outcome, RefusedReason, Stage};

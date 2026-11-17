//! What the IM tests of this package share that the `zx-testing` crate cannot hold: it uses
//! the types of the IM crates, and `zx-testing` depends on none of them. Their other helpers,
//! such as the fake chat platforms, are in `zx-testing`.
//!
//! - `im`: running a channel against the IM service

pub(crate) mod im;

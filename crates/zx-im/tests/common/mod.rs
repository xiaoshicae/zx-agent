//! What the channel tests of this package (`im_slack`, `im_feishu`) share that the
//! `zx-testing` crate cannot hold: it uses the types of `zx-im`, which `zx-testing` does not
//! depend on. Their other helpers, such as the fake chat platforms, are in `zx-testing`.
//!
//! - `im`: running a channel against the IM service

pub(crate) mod im;

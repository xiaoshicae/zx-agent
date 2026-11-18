//! Small helpers the crates of the zx-agent workspace share. Not part of the zx-agent API.
//!
//! A helper belongs here when it means nothing in the agent's domain, more than one crate
//! uses it, and it needs only the standard library and tokio. The last keeps every crate, the
//! kernel included, free to depend on this one; a helper that needs more, such as `libc` for
//! reading files without blocking on a FIFO, stays with the crate that owns that concern.
//!
//! - [`sync`]: a lock that recovers from poisoning, and a task aborted when it is dropped
//! - [`text`]: text bounded by characters or bytes, cut on character boundaries
//! - [`random`]: randomness for spreading retries and reconnections
//! - [`default`]: [`default::is_default`], for leaving a type's default, such as a zero count,
//!   out of serialized records

pub mod default;
pub mod random;
pub mod sync;
pub mod text;

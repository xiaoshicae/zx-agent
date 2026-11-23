//! Small helpers the crates of the zx-agent workspace share. Not part of the zx-agent API.
//!
//! A helper belongs here when it means nothing in the agent's domain, more than one crate
//! uses it, and it needs only the standard library and tokio. The last keeps every crate, the
//! kernel included, free to depend on this one; a helper that needs more, such as `libc` for
//! reading files without blocking on a FIFO, stays with the crate that owns that concern.
//!
//! - [`sync`]: a lock that recovers from poisoning, and a task aborted when it is dropped
//! - [`text`]: text bounded by characters or bytes, cut on character boundaries, and
//!   amounts that keep their first digits however small
//! - [`random`]: randomness for spreading retries and reconnections
//! - [`default`]: [`default::is_default`], for leaving a type's default, such as a zero count,
//!   out of serialized records
//! - [`env`](mod@env): [`env::env_value`], an environment variable that counts only when it
//!   is set and not empty, and [`env::expand`], a value of a configuration file whose
//!   `${NAME}` placeholders are replaced by variables
//! - [`registry`]: implementations registered under names, for a configuration to choose one
//! - [`stdio`]: lines on stdout and stderr that do not panic when the stream is closed

pub mod default;
pub mod env;
pub mod random;
pub mod registry;
pub mod stdio;
pub mod sync;
pub mod text;

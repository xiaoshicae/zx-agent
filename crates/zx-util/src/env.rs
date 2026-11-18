//! Environment variables that count only when they hold something.

/// The value of the environment variable `name`, when it is set and not empty.
///
/// A value that is not UTF-8 holds no usable key, token, address or path: it counts as unset.
pub fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

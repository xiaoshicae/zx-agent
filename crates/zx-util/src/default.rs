//! Helpers on a type's default value.

/// Whether `value` is its type's default: for a field that
/// `#[serde(skip_serializing_if = "zx_util::default::is_default")]` leaves out of a record
/// while it holds the default, such as a zero count or a setting that was not changed.
pub fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

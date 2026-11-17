//! Helpers on numbers.

/// Whether `value` is zero, its type's default: for a count that
/// `#[serde(skip_serializing_if = "zx_util::num::is_zero")]` leaves out of a record while it is.
pub fn is_zero<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

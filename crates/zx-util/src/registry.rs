//! Implementations registered under names, for a configuration to choose one by its name:
//! the built-in ones, then those a host adds.

use std::collections::HashSet;
use std::sync::Arc;

/// Implementations of `T`, each under a name, in the order they were registered.
pub struct Registry<T: ?Sized> {
    /// What the implementations are, as errors name them, such as `provider`.
    what: &'static str,
    entries: Vec<(String, Arc<T>)>,
}

impl<T: ?Sized> Registry<T> {
    /// A registry of `what`, holding `entries`, `(name, implementation)` pairs.
    pub fn new(what: &'static str, entries: Vec<(String, Arc<T>)>) -> Self {
        Self { what, entries }
    }

    /// Add `entry` under `name`. A name taken already keeps its first entry;
    /// [`check_names`](Self::check_names) reports it.
    pub fn register(&mut self, name: &str, entry: Arc<T>) {
        self.entries.push((name.to_string(), entry));
    }

    /// The implementation registered under `name`. Fails when there is none, naming those
    /// there are.
    pub fn get(&self, name: &str) -> Result<&Arc<T>, String> {
        match self.entries.iter().find(|(registered, _)| registered == name) {
            Some((_, entry)) => Ok(entry),
            None => {
                let names: Vec<&str> = self.entries.iter().map(|(name, _)| name.as_str()).collect();
                Err(format!(
                    "no {} is registered under the name {name:?}; use one of {}",
                    self.what,
                    names.join(", ")
                ))
            }
        }
    }

    /// Fails when two implementations are registered under one name.
    pub fn check_names(&self) -> Result<(), String> {
        let mut seen = HashSet::new();
        match self.entries.iter().find(|(name, _)| !seen.insert(name.as_str())) {
            Some((name, _)) => Err(format!(
                "more than one {} is registered under the name {name:?}",
                self.what
            )),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn colors() -> Registry<str> {
        Registry::new(
            "color",
            vec![
                ("red".to_string(), Arc::from("#f00")),
                ("blue".to_string(), Arc::from("#00f")),
            ],
        )
    }

    #[test]
    fn an_implementation_is_found_by_its_name_and_a_missing_one_names_those_there_are() {
        let mut registry = colors();
        registry.register("green", Arc::from("#0f0"));
        assert_eq!(registry.get("green").map(|entry| &**entry), Ok("#0f0"));
        assert_eq!(
            registry.get("pink").err().unwrap(),
            "no color is registered under the name \"pink\"; use one of red, blue, green"
        );
        assert_eq!(registry.check_names(), Ok(()));
    }

    #[test]
    fn a_name_registered_twice_keeps_its_first_entry_and_is_reported() {
        let mut registry = colors();
        registry.register("red", Arc::from("#e00"));
        assert_eq!(registry.get("red").map(|entry| &**entry), Ok("#f00"));
        assert_eq!(
            registry.check_names().err().unwrap(),
            "more than one color is registered under the name \"red\""
        );
    }
}

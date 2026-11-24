//! The models config.toml lists (`[[models]]`): the choices the model option offers, and the
//! context windows the usage updates measure against.

use std::collections::{BTreeMap, HashMap};

use zx_agent::ai::ModelMeta;

/// The models config.toml lists, by ID in order, with their context windows when known.
pub(super) struct Models {
    windows: BTreeMap<String, Option<u64>>,
}

impl Models {
    /// The models of `table`, as the application read them from config.toml.
    pub(super) fn new(table: &HashMap<String, ModelMeta>) -> Self {
        Self {
            windows: table
                .iter()
                .map(|(id, meta)| (id.clone(), meta.context_window))
                .collect(),
        }
    }

    /// The IDs of the listed models, in order.
    pub(super) fn ids(&self) -> impl Iterator<Item = &str> {
        self.windows.keys().map(String::as_str)
    }

    /// Whether config.toml lists `model`.
    pub(super) fn lists(&self, model: &str) -> bool {
        self.windows.contains_key(model)
    }

    /// The context window of `model`, in tokens; `None` when config.toml gives none.
    pub(super) fn window(&self, model: &str) -> Option<u64> {
        self.windows.get(model).copied().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_are_listed_in_order_with_the_windows_config_gives() {
        let meta = |window| ModelMeta {
            context_window: window,
            ..ModelMeta::default()
        };
        let table = HashMap::from([("b".to_string(), meta(None)), ("a".to_string(), meta(Some(1000)))]);
        let models = Models::new(&table);
        assert_eq!(models.ids().collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(models.window("a"), Some(1000));
        assert_eq!(models.window("b"), None);
        assert_eq!(models.window("c"), None);
        assert!(models.lists("b") && !models.lists("c"));
    }
}

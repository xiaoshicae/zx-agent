//! The placeholders of the configuration files: every string a file holds may name
//! environment variables as `${NAME}`, replaced as the file is read ([`zx_util::env::expand`]).
//! The credentials among the strings are told apart by their keys, so that the variables they
//! name are hidden from commands and their values redacted from tool output.

use anyhow::{Result, anyhow};
use zx_util::env::expand;

/// What a string of a configuration file holds, by its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holds {
    /// Text, whatever it names.
    Text,
    /// A credential, named by variables or written as it is.
    Credential,
    /// The value of a request header: a credential when it names a variable, as a token in it
    /// would; text when written as it is, as most headers are.
    Header,
    /// Left as written, for its key's owner to expand: the API key, which goes without when
    /// its variable has no value.
    Kept,
}

/// The credentials a configuration holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credentials {
    /// The variables they name, which commands must not see.
    pub variables: Vec<String>,
    /// Their values, from variables or written as they are, to redact from tool output.
    pub values: Vec<String>,
}

impl Credentials {
    /// Add `others`, leaving out what is here already.
    pub fn extend(&mut self, others: Credentials) {
        for variable in others.variables {
            if !self.variables.contains(&variable) {
                self.variables.push(variable);
            }
        }
        for value in others.values {
            // An empty value holds nothing to redact.
            if !value.is_empty() && !self.values.contains(&value) {
                self.values.push(value);
            }
        }
    }
}

/// Replace the placeholders of every string in `table`, a file or a section of one, telling
/// what each holds by `holds`, which is given the keys from `table` down to the string,
/// without the indexes of arrays: the credentials found. Fails for a malformed placeholder,
/// and for a variable without a value; the error names the key, with the indexes.
pub fn expand_table(table: &mut toml::Table, holds: &dyn Fn(&[&str]) -> Holds) -> Result<Credentials> {
    let mut found = Credentials::default();
    let mut path = Vec::new();
    for (key, value) in table.iter_mut() {
        path.push(Step::Key(key.clone()));
        expand_value(value, &mut path, holds, &mut found)?;
        path.pop();
    }
    Ok(found)
}

/// A step from a table down to one of its values.
enum Step {
    Key(String),
    Index(usize),
}

/// The placeholders of `value`, at `path`, replaced, and its credentials added to `found`.
fn expand_value(
    value: &mut toml::Value,
    path: &mut Vec<Step>,
    holds: &dyn Fn(&[&str]) -> Holds,
    found: &mut Credentials,
) -> Result<()> {
    match value {
        toml::Value::String(text) => expand_text(text, path, holds, found),
        toml::Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                path.push(Step::Index(index));
                expand_value(item, path, holds, found)?;
                path.pop();
            }
            Ok(())
        }
        toml::Value::Table(table) => {
            for (key, item) in table.iter_mut() {
                path.push(Step::Key(key.clone()));
                expand_value(item, path, holds, found)?;
                path.pop();
            }
            Ok(())
        }
        // They name no variable.
        toml::Value::Integer(_) | toml::Value::Float(_) | toml::Value::Boolean(_) | toml::Value::Datetime(_) => Ok(()),
    }
}

/// The placeholders of `text`, at `path`, replaced, and the credential it is added to `found`.
fn expand_text(
    text: &mut String,
    path: &[Step],
    holds: &dyn Fn(&[&str]) -> Holds,
    found: &mut Credentials,
) -> Result<()> {
    let keys: Vec<&str> = path
        .iter()
        .filter_map(|step| match step {
            Step::Key(key) => Some(key.as_str()),
            Step::Index(_) => None,
        })
        .collect();
    let holds = holds(&keys);
    if holds == Holds::Kept {
        return Ok(());
    }
    let expanded = expand(text).map_err(|error| anyhow!("{}: {error}", shown(path)))?;
    let credential = match holds {
        Holds::Credential => true,
        Holds::Header => !expanded.variables.is_empty(),
        Holds::Text | Holds::Kept => false,
    };
    if credential {
        found.extend(Credentials {
            variables: expanded.variables,
            values: vec![expanded.value.clone()],
        });
    }
    *text = expanded.value;
    Ok(())
}

/// `path` as a key of the file: `mcp[1].headers.X-Api-Key`.
fn shown(path: &[Step]) -> String {
    let mut shown = String::new();
    for step in path {
        match step {
            Step::Key(key) => {
                if !shown.is_empty() {
                    shown.push('.');
                }
                shown.push_str(key);
            }
            Step::Index(index) => shown.push_str(&format!("[{index}]")),
        }
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the strings of the test file hold: `secret` is a credential, the values of
    /// `headers` are headers, `kept` is kept.
    fn holds(keys: &[&str]) -> Holds {
        match keys {
            [.., "secret"] => Holds::Credential,
            [.., "headers", _] => Holds::Header,
            ["kept"] => Holds::Kept,
            _ => Holds::Text,
        }
    }

    #[test]
    fn every_string_is_expanded_and_only_credentials_are_kept_apart() {
        // A variable every environment has, so that no test sets one.
        let path = std::env::var("PATH").unwrap();
        let mut table: toml::Table = toml::from_str(
            "plain = \"${PATH}\"\nkept = \"${PATH}\"\nsecret = \"literal-secret\"\n\
             [[servers]]\nsecret = \"x${PATH}\"\nheaders = { A = \"team-a\", B = \"Bearer ${PATH}\" }\n\
             numbers = [1, \"$${PATH}\"]\n",
        )
        .unwrap();
        let found = expand_table(&mut table, &holds).unwrap();
        assert_eq!(table["plain"].as_str(), Some(path.as_str()));
        assert_eq!(table["kept"].as_str(), Some("${PATH}"));
        let server = &table["servers"][0];
        assert_eq!(server["numbers"][1].as_str(), Some("${PATH}"));
        assert_eq!(server["headers"]["A"].as_str(), Some("team-a"));
        assert_eq!(found.variables, ["PATH"]);
        assert_eq!(
            found.values,
            [
                // In the order of the keys, which a table keeps sorted.
                "literal-secret".to_string(),
                format!("Bearer {path}"),
                format!("x{path}")
            ]
        );
    }

    #[test]
    fn an_error_names_the_key_with_the_indexes() {
        let mut table: toml::Table =
            toml::from_str("[[servers]]\n[[servers]]\nheaders = { B = \"${ZX_APP_TEST_UNSET_VARIABLE}\" }\n").unwrap();
        let error = expand_table(&mut table, &holds).unwrap_err().to_string();
        assert_eq!(
            error,
            "servers[1].headers.B: names ${ZX_APP_TEST_UNSET_VARIABLE}, which is not set"
        );
        let mut table: toml::Table = toml::from_str("plain = \"${broken\"").unwrap();
        assert!(
            expand_table(&mut table, &holds)
                .unwrap_err()
                .to_string()
                .starts_with("plain: ${broken is no placeholder")
        );
    }
}

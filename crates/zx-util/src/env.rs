//! Environment variables that count only when they hold something, and the values of
//! configuration files that name them as `${NAME}` (pi: `resolveConfigValue`, without `$NAME`
//! and `!command`).

use std::fmt;

/// The value of the environment variable `name`, when it is set and not empty.
///
/// A value that is not UTF-8 holds no usable key, token, address or path: it counts as unset.
pub fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// A value with its placeholders replaced, and the variables it named.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expanded {
    pub value: String,
    /// The variables `${NAME}` named, in the order written, each once.
    pub variables: Vec<String>,
}

/// Why a value's placeholders cannot be replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unexpandable {
    /// A `${` without its `}`, or with something else than a variable's name before it, as
    /// written.
    Malformed(String),
    /// A variable named that is not set, or is empty.
    Unset(String),
}

impl fmt::Display for Unexpandable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(written) => write!(
                formatter,
                "{written} is no placeholder: write ${{NAME}}, NAME of letters, digits and _, or $$ for a $"
            ),
            Self::Unset(name) => write!(formatter, "names ${{{name}}}, which is not set"),
        }
    }
}

/// `text` with each `${NAME}` replaced by the value of the environment variable NAME
/// ([`env_value`]), and each `$$` by `$`; any other `$` stays as it is. Fails for a malformed
/// placeholder and for a variable that is not set.
pub fn expand(text: &str) -> Result<Expanded, Unexpandable> {
    expand_with(text, env_value)
}

/// [`expand`], with `lookup` giving the value of a variable, `None` when it has none.
pub fn expand_with(text: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<Expanded, Unexpandable> {
    let mut expanded = Expanded {
        value: String::with_capacity(text.len()),
        variables: Vec::new(),
    };
    for part in parts(text)? {
        match part {
            Part::Text(text) => expanded.value.push_str(text),
            Part::Variable(name) => {
                let value = lookup(name).ok_or_else(|| Unexpandable::Unset(name.to_string()))?;
                expanded.value.push_str(&value);
                if !expanded.variables.iter().any(|known| known == name) {
                    expanded.variables.push(name.to_string());
                }
            }
        }
    }
    Ok(expanded)
}

/// The variables `text` names, in the order written, each once, whether or not they are set.
/// Fails for a malformed placeholder.
pub fn variables(text: &str) -> Result<Vec<String>, Unexpandable> {
    let mut names: Vec<String> = Vec::new();
    for part in parts(text)? {
        if let Part::Variable(name) = part
            && !names.iter().any(|known| known == name)
        {
            names.push(name.to_string());
        }
    }
    Ok(names)
}

/// A piece of a value: text as it is, or a variable's place.
enum Part<'a> {
    Text(&'a str),
    Variable(&'a str),
}

/// The pieces of `text`, `$$` standing for one `$`.
fn parts(text: &str) -> Result<Vec<Part<'_>>, Unexpandable> {
    let mut parts = Vec::new();
    let mut rest = text;
    while let Some(dollar) = rest.find('$') {
        let (before, from_dollar) = rest.split_at(dollar);
        parts.push(Part::Text(before));
        let after = &from_dollar[1..];
        if let Some(beyond) = after.strip_prefix('$') {
            parts.push(Part::Text("$"));
            rest = beyond;
        } else if let Some(inside) = after.strip_prefix('{') {
            let end = inside
                .find('}')
                .ok_or_else(|| Unexpandable::Malformed(from_dollar.to_string()))?;
            let name = &inside[..end];
            if !is_name(name) {
                return Err(Unexpandable::Malformed(format!("${{{name}}}")));
            }
            parts.push(Part::Variable(name));
            rest = &inside[end + 1..];
        } else {
            parts.push(Part::Text("$"));
            rest = after;
        }
    }
    parts.push(Part::Text(rest));
    Ok(parts)
}

/// Whether `name` can name an environment variable here: letters, digits and `_`, not
/// starting with a digit.
fn is_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The variables of the tests: `KEY` and `HOST` are set, nothing else is.
    fn lookup(name: &str) -> Option<String> {
        match name {
            "KEY" => Some("sk-1".into()),
            "HOST" => Some("proxy.internal".into()),
            _ => None,
        }
    }

    #[test]
    fn a_placeholder_is_replaced_by_its_variable_and_the_variables_are_said_once() {
        let expanded = expand_with("http://${HOST}:3128/${KEY}?k=${KEY}", lookup).unwrap();
        assert_eq!(expanded.value, "http://proxy.internal:3128/sk-1?k=sk-1");
        assert_eq!(expanded.variables, ["HOST", "KEY"]);
        assert_eq!(variables("${KEY}${HOST}${KEY}").unwrap(), ["KEY", "HOST"]);
    }

    #[test]
    fn a_dollar_is_written_as_two_and_any_other_dollar_stays_as_it_is() {
        let expanded = expand_with("pa$$word $5 cost$ $HOME 多字节$", lookup).unwrap();
        assert_eq!(expanded.value, "pa$word $5 cost$ $HOME 多字节$");
        assert!(expanded.variables.is_empty());
        assert_eq!(expand_with("$${KEY}", lookup).unwrap().value, "${KEY}");
    }

    #[test]
    fn a_malformed_placeholder_and_an_unset_variable_are_refused() {
        for (text, expected) in [
            ("${KEY", Unexpandable::Malformed("${KEY".into())),
            ("x ${}", Unexpandable::Malformed("${}".into())),
            ("${1KEY}", Unexpandable::Malformed("${1KEY}".into())),
            ("${KEY-1}", Unexpandable::Malformed("${KEY-1}".into())),
            ("${MISSING}", Unexpandable::Unset("MISSING".into())),
        ] {
            assert_eq!(expand_with(text, lookup), Err(expected), "{text}");
        }
        assert_eq!(
            Unexpandable::Unset("KEY".into()).to_string(),
            "names ${KEY}, which is not set"
        );
        // Unset does not matter to which variables are named.
        assert_eq!(variables("${MISSING}").unwrap(), ["MISSING"]);
        assert!(variables("${MISSING").is_err());
    }
}

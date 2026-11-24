//! Environment variables that count only when they hold something, and the values of
//! configuration files that name them as `${NAME}`, with a default as the shell and Docker
//! Compose write one, `${NAME:-default}` or `${NAME-default}` (pi: `resolveConfigValue`,
//! without its `!command`; pi has no defaults). pi's `$NAME` is refused rather than taken as
//! text, so that a value written for the shell fails where it is read instead of where it is
//! used, such as an API key sent as `$OPENAI_API_KEY`.

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
    /// The variables the placeholders named, in the order written, each once, whether their
    /// value or a default took their place.
    pub variables: Vec<String>,
}

/// Why a value's placeholders cannot be replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unexpandable {
    /// A `${` without its `}`, or with something else than a variable's name and a default
    /// before it, as written.
    Malformed(String),
    /// A `$` followed by a variable's name, the shell's `$NAME`: `${NAME}` here, or `$$NAME`
    /// for the text. Holds the name.
    Unbraced(String),
    /// A variable named without a default that is not set, or is empty.
    Unset(String),
}

impl fmt::Display for Unexpandable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(written) => write!(
                formatter,
                "{written} is no placeholder: write ${{NAME}}, ${{NAME:-default}} or ${{NAME-default}}, NAME of letters, digits and _ and the default without $; or $$ for a $"
            ),
            Self::Unbraced(name) => write!(
                formatter,
                "${name} is no placeholder: write ${{{name}}} for the variable, or $${name} for the text"
            ),
            Self::Unset(name) => write!(formatter, "names ${{{name}}}, which is not set"),
        }
    }
}

/// `text` with each placeholder replaced, and each `$$` by `$`; any other `$` stays as it is,
/// but for one followed by a variable's name, the shell's `$NAME`. `${NAME}` takes the value
/// of the environment variable NAME, which must be set and not empty; `${NAME:-default}`
/// takes `default` when NAME is unset or empty, `${NAME-default}` only when it is unset. A
/// value that is not UTF-8 counts as unset. Fails for a malformed placeholder, for `$NAME`
/// and for a variable that has neither a value nor a default.
pub fn expand(text: &str) -> Result<Expanded, Unexpandable> {
    // Unlike for `env_value`, an empty value is one: `${NAME-default}` keeps it. A value that
    // is not UTF-8 is none, as there.
    expand_with(text, |name| std::env::var(name).ok())
}

/// [`expand`], with `lookup` giving the value of a variable: `None` when it is unset, and an
/// empty string when it is set but empty.
pub fn expand_with(text: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<Expanded, Unexpandable> {
    let mut expanded = Expanded {
        value: String::with_capacity(text.len()),
        variables: Vec::new(),
    };
    for part in parts(text)? {
        match part {
            Part::Text(text) => expanded.value.push_str(text),
            Part::Variable(name, fallback) => {
                expanded.value.push_str(&fallback.replacing(name, lookup(name))?);
                if !expanded.variables.iter().any(|known| known == name) {
                    expanded.variables.push(name.to_string());
                }
            }
        }
    }
    Ok(expanded)
}

/// The variables `text` names, in the order written, each once, whether or not they are set
/// and whether or not they have a default. Fails for a malformed placeholder and for `$NAME`.
pub fn variables(text: &str) -> Result<Vec<String>, Unexpandable> {
    let mut names: Vec<String> = Vec::new();
    for part in parts(text)? {
        if let Part::Variable(name, _) = part
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
    Variable(&'a str, Fallback<'a>),
}

/// What takes a variable's place when it has no value.
enum Fallback<'a> {
    /// Nothing: `${NAME}` needs a value, and an empty one is none.
    Required,
    /// `${NAME:-default}`: the default, when the variable is unset or empty.
    IfUnsetOrEmpty(&'a str),
    /// `${NAME-default}`: the default, only when the variable is unset.
    IfUnset(&'a str),
}

impl Fallback<'_> {
    /// What takes the place of the variable `name`, whose value is `value`.
    fn replacing(&self, name: &str, value: Option<String>) -> Result<String, Unexpandable> {
        match (value, self) {
            (Some(value), Self::IfUnset(_)) => Ok(value),
            (Some(value), Self::Required | Self::IfUnsetOrEmpty(_)) if !value.is_empty() => Ok(value),
            (_, Self::IfUnsetOrEmpty(default) | Self::IfUnset(default)) => Ok((*default).to_string()),
            (_, Self::Required) => Err(Unexpandable::Unset(name.to_string())),
        }
    }

    /// The default, empty for none.
    fn default(&self) -> &str {
        match self {
            Self::Required => "",
            Self::IfUnsetOrEmpty(default) | Self::IfUnset(default) => default,
        }
    }
}

/// The pieces of `text`, `$$` standing for one `$`. A `$` before a variable's name is
/// refused.
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
            let within = &inside[..end];
            let (name, fallback) =
                placeholder(within).ok_or_else(|| Unexpandable::Malformed(format!("${{{within}}}")))?;
            parts.push(Part::Variable(name, fallback));
            rest = &inside[end + 1..];
        } else if let Some(name) = unbraced_name(after) {
            return Err(Unexpandable::Unbraced(name.to_string()));
        } else {
            parts.push(Part::Text("$"));
            rest = after;
        }
    }
    parts.push(Part::Text(rest));
    Ok(parts)
}

/// The variable and the default of the placeholder `${within}`, when it is one.
fn placeholder(within: &str) -> Option<(&str, Fallback<'_>)> {
    let name_end = within
        .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        // Nothing but the name.
        .unwrap_or(within.len());
    let (name, rest) = within.split_at(name_end);
    let fallback = if rest.is_empty() {
        Fallback::Required
    } else if let Some(default) = rest.strip_prefix(":-") {
        Fallback::IfUnsetOrEmpty(default)
    } else {
        Fallback::IfUnset(rest.strip_prefix('-')?)
    };
    // A `$` in a default would read as a placeholder in it, which is not expanded.
    (is_name(name) && !fallback.default().contains('$')).then_some((name, fallback))
}

/// The variable's name that `after`, what follows a `$`, starts with, as the shell reads
/// `$NAME`: a letter or `_`, then letters, digits and `_`.
fn unbraced_name(after: &str) -> Option<&str> {
    let first = after.chars().next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    let end = after
        .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .unwrap_or(after.len());
    Some(&after[..end])
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

    /// The variables of the tests: `KEY` and `HOST` are set, `EMPTY` is set but empty,
    /// nothing else is set.
    fn lookup(name: &str) -> Option<String> {
        match name {
            "KEY" => Some("sk-1".into()),
            "HOST" => Some("proxy.internal".into()),
            "EMPTY" => Some(String::new()),
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
    fn a_default_takes_the_place_of_an_unset_variable_and_with_a_colon_of_an_empty_one() {
        for (text, expected) in [
            ("${MISSING:-d}", "d"),
            ("${EMPTY:-d}", "d"),
            ("${KEY:-d}", "sk-1"),
            ("${MISSING-d}", "d"),
            ("${EMPTY-d}", ""),
            ("${KEY-d}", "sk-1"),
            ("${MISSING:-}", ""),
            ("${MISSING-http://h:1/a-b c 默认}", "http://h:1/a-b c 默认"),
        ] {
            assert_eq!(expand_with(text, lookup).unwrap().value, expected, "{text}");
        }
        // A variable with a default is named all the same, for a credential to hide it.
        let expanded = expand_with("${MISSING:-x}${KEY-y}", lookup).unwrap();
        assert_eq!(expanded.variables, ["MISSING", "KEY"]);
        assert_eq!(variables("${MISSING:-x}${KEY-y}").unwrap(), ["MISSING", "KEY"]);
        // The environment: PATH is set, ZX_UTIL_TEST_UNSET is not.
        assert_eq!(expand("${ZX_UTIL_TEST_UNSET:-d}").unwrap().value, "d");
        assert_eq!(expand("${PATH-d}").unwrap().value, std::env::var("PATH").unwrap());
    }

    #[test]
    fn a_dollar_is_written_as_two_and_one_before_no_name_stays_as_it_is() {
        let expanded = expand_with("pa$$word $5 cost$ $-x $.多字节$", lookup).unwrap();
        assert_eq!(expanded.value, "pa$word $5 cost$ $-x $.多字节$");
        assert!(expanded.variables.is_empty());
        assert_eq!(expand_with("$${KEY}", lookup).unwrap().value, "${KEY}");
        assert_eq!(expand_with("$$HOME", lookup).unwrap().value, "$HOME");
    }

    #[test]
    fn a_name_after_a_dollar_is_refused_for_want_of_braces() {
        for (text, name) in [
            ("$OPENAI_API_KEY", "OPENAI_API_KEY"),
            ("Bearer $KEY", "KEY"),
            ("${HOST}:$_PORT1/x", "_PORT1"),
        ] {
            assert_eq!(
                expand_with(text, lookup),
                Err(Unexpandable::Unbraced(name.into())),
                "{text}"
            );
            assert_eq!(variables(text), Err(Unexpandable::Unbraced(name.into())), "{text}");
        }
        assert_eq!(
            Unexpandable::Unbraced("KEY".into()).to_string(),
            "$KEY is no placeholder: write ${KEY} for the variable, or $$KEY for the text"
        );
    }

    #[test]
    fn a_malformed_placeholder_and_a_variable_without_value_or_default_are_refused() {
        for (text, expected) in [
            ("${KEY", Unexpandable::Malformed("${KEY".into())),
            ("x ${}", Unexpandable::Malformed("${}".into())),
            ("${1KEY}", Unexpandable::Malformed("${1KEY}".into())),
            // The shell's substring and its other forms are not defaults here.
            ("${KEY:1}", Unexpandable::Malformed("${KEY:1}".into())),
            ("${KEY:=d}", Unexpandable::Malformed("${KEY:=d}".into())),
            ("${:-d}", Unexpandable::Malformed("${:-d}".into())),
            // Nor is a placeholder within a default.
            ("${KEY:-${HOST}}", Unexpandable::Malformed("${KEY:-${HOST}".into())),
            ("${KEY-a$$b}", Unexpandable::Malformed("${KEY-a$$b}".into())),
            ("${MISSING}", Unexpandable::Unset("MISSING".into())),
            ("${EMPTY}", Unexpandable::Unset("EMPTY".into())),
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

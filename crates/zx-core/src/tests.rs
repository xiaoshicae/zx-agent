//! The layering the module list of the crate's documentation states, checked against the
//! source: the `crate::` paths of each module's production code, outside comments, tests and
//! the files compiled for tests only.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

fn source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn lib() -> String {
    fs::read_to_string(source().join("lib.rs")).unwrap()
}

/// The modules of the list in `lib.rs`, in its order, each with the modules it may use: those
/// in brackets, every one before it for "all of the above", none without brackets. The
/// modules compiled for tests only are left out.
fn stated_layers(lib: &str) -> Vec<(String, BTreeSet<String>)> {
    let mut layers: Vec<(String, BTreeSet<String>)> = Vec::new();
    for line in lib.lines() {
        let Some(entry) = line.strip_prefix("//! - ") else {
            continue;
        };
        let entry = entry.strip_prefix('[').unwrap_or(entry);
        let Some((name, rest)) = entry.strip_prefix('`').and_then(|entry| entry.split_once('`')) else {
            continue;
        };
        let rest = rest.strip_prefix(']').unwrap_or(rest);
        let uses = match rest.strip_prefix(" (").and_then(|rest| rest.split_once(')')) {
            Some(("all of the above", _)) => layers.iter().map(|(name, _)| name.clone()).collect(),
            Some((names, _)) => names.split(", ").map(str::to_string).collect(),
            None if rest.starts_with(':') => BTreeSet::new(),
            // Written "`name`, compiled for tests only".
            None => continue,
        };
        layers.push((name.to_string(), uses));
    }
    layers
}

/// The items the crate root re-exports, by name, with the module each comes from.
fn reexports(lib: &str) -> BTreeMap<String, String> {
    let mut items = BTreeMap::new();
    for line in lib.lines() {
        let Some((module, names)) = line
            .strip_prefix("pub use self::")
            .and_then(|path| path.trim_end_matches(';').split_once("::"))
        else {
            continue;
        };
        for name in names.trim_matches(['{', '}']).split(", ") {
            items.insert(name.to_string(), module.to_string());
        }
    }
    items
}

/// The modules `lib.rs` declares, but for those compiled for tests only.
fn declared_modules(lib: &str) -> BTreeSet<String> {
    let mut modules = BTreeSet::new();
    let mut for_tests = false;
    for line in lib.lines() {
        let declared = line.strip_prefix("pub mod ").or_else(|| line.strip_prefix("mod "));
        if let Some(name) = declared.and_then(|rest| rest.strip_suffix(';'))
            && !for_tests
        {
            modules.insert(name.to_string());
        }
        for_tests = line == "#[cfg(test)]";
    }
    modules
}

/// The lines of the production code of module `name`: its file and those of its submodules,
/// without comments, inline test modules and the files compiled for tests only.
fn production_lines(name: &str) -> Vec<String> {
    let mut files = vec![source().join(format!("{name}.rs"))];
    let mut directories = vec![source().join(name)];
    while let Some(directory) = directories.pop() {
        if !directory.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                directories.push(path);
            } else if !path.ends_with("tests.rs") && !path.ends_with("test_support.rs") {
                files.push(path);
            }
        }
    }
    let mut lines = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).unwrap();
        let mut after_cfg_test = false;
        for line in text.lines() {
            let code = line.trim_start();
            if after_cfg_test && code.split_whitespace().any(|word| word == "mod") && code.ends_with('{') {
                break;
            }
            after_cfg_test = code == "#[cfg(test)]";
            if !code.starts_with("//") {
                lines.push(code.to_string());
            }
        }
    }
    lines
}

/// What follows each `crate::` on `line`: a module, an item the crate root re-exports, named
/// by its module, or another name of the crate root.
fn crate_paths<'a>(line: &'a str, reexports: &'a BTreeMap<String, String>) -> impl Iterator<Item = &'a str> {
    line.match_indices("crate::").filter_map(move |(at, prefix)| {
        // Part of a longer name, such as `zx_crate::`.
        if line[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_') {
            return None;
        }
        let rest = &line[at + prefix.len()..];
        let end = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let name = &rest[..end];
        Some(reexports.get(name).map_or(name, String::as_str))
    })
}

#[test]
fn the_module_list_names_every_module() {
    let lib = lib();
    let listed: BTreeSet<String> = stated_layers(&lib).into_iter().map(|(name, _)| name).collect();
    assert_eq!(listed, declared_modules(&lib));
}

#[test]
fn each_module_uses_only_the_modules_the_list_names_for_it() {
    let lib = lib();
    let layers = stated_layers(&lib);
    let reexports = reexports(&lib);
    let modules: BTreeSet<&str> = layers.iter().map(|(name, _)| name.as_str()).collect();
    let mut violations = Vec::new();
    for (name, allowed) in &layers {
        let lines = production_lines(name);
        let used: BTreeSet<String> = lines
            .iter()
            .flat_map(|line| crate_paths(line, &reexports))
            .filter(|used| used != name && modules.contains(used))
            .map(str::to_string)
            .collect();
        for used in used.difference(allowed) {
            violations.push(format!(
                "{name} uses {used}, which the list in lib.rs does not name for it"
            ));
        }
    }
    assert!(violations.is_empty(), "{violations:#?}");
}

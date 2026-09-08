//! Every `[profile.*.package.<name>]` entry must name a package cargo can see.
//!
//! A profile spec that matches nothing is not an error: cargo prints
//!
//!   warning: profile package spec `zeno` in profile `selfdev` did not match any packages
//!   help: a package with a similar name exists: `aes`
//!
//! and carries on. That means a pinned crate can leave the dependency graph
//! (`cosmic-text`, `swash`, `yazi`, `zeno` and `unicode-linebreak` all did when
//! the legacy desktop crate was removed) while the pin lingers, and the only
//! symptom is ~25 lines of noise ahead of every single build. Warning spam of
//! that size is how a real warning gets missed, so this asserts the property
//! directly instead of relying on anyone reading build output.
//!
//! Cargo.lock is the oracle: it lists every package in the resolved graph for
//! all targets and features, so a crate used only by desktop2, only on Linux,
//! or only by a dev-dependency still counts as resolvable.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Package names present in the resolved dependency graph.
fn locked_package_names(lock_path: &Path) -> HashSet<String> {
    let text = std::fs::read_to_string(lock_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", lock_path.display()));
    let lock: toml::Value = text
        .parse()
        .unwrap_or_else(|err| panic!("parse {}: {err}", lock_path.display()));

    let packages = lock
        .get("package")
        .and_then(toml::Value::as_array)
        .unwrap_or_else(|| panic!("{} has no [[package]] entries", lock_path.display()));

    let names: HashSet<String> = packages
        .iter()
        .filter_map(|package| package.get("name")?.as_str().map(str::to_owned))
        .collect();

    assert!(
        names.contains("jcode"),
        "Cargo.lock parsed but does not contain the root package; parsing is wrong, \
         not the manifest"
    );
    names
}

/// Every `<name>` in `[profile.<profile>.package.<name>]`, paired with its profile.
fn profile_package_specs(manifest_path: &Path) -> Vec<(String, String)> {
    let text = std::fs::read_to_string(manifest_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", manifest_path.display()));
    let manifest: toml::Value = text
        .parse()
        .unwrap_or_else(|err| panic!("parse {}: {err}", manifest_path.display()));

    let Some(profiles) = manifest.get("profile").and_then(toml::Value::as_table) else {
        panic!("{} declares no [profile.*] tables", manifest_path.display());
    };

    let mut specs = Vec::new();
    for (profile_name, profile) in profiles {
        let Some(packages) = profile.get("package").and_then(toml::Value::as_table) else {
            continue;
        };
        for spec in packages.keys() {
            specs.push((profile_name.clone(), spec.clone()));
        }
    }
    specs
}

/// The specs cargo would warn about, as `[profile.<p>.package.<s>]` strings.
///
/// Pure so the rule itself can be tested against a planted bad pin, without
/// editing Cargo.toml (which would force a full workspace rebuild).
fn unmatched_specs(specs: &[(String, String)], locked: &HashSet<String>) -> Vec<String> {
    let mut unmatched: Vec<String> = specs
        .iter()
        // `*` is the documented wildcard spec, not a package name.
        .filter(|(_, spec)| spec != "*")
        .filter(|(_, spec)| !locked.contains(spec.as_str()))
        .map(|(profile, spec)| format!("[profile.{profile}.package.{spec}]"))
        .collect();
    unmatched.sort();
    unmatched
}

#[test]
fn every_profile_package_pin_matches_a_real_package() {
    let root = workspace_root();
    let specs = profile_package_specs(&root.join("Cargo.toml"));
    assert!(
        specs.len() > 20,
        "expected the workspace to pin many packages, found {}; the parser is probably \
         reading the wrong table",
        specs.len()
    );

    let locked = locked_package_names(&root.join("Cargo.lock"));
    let unmatched = unmatched_specs(&specs, &locked);

    assert!(
        unmatched.is_empty(),
        "these profile package pins match no package in Cargo.lock, so cargo prints a \
         `did not match any packages` warning on every build.\n\
         Delete them from Cargo.toml, or add the dependency back:\n  {}",
        unmatched.join("\n  ")
    );
}

/// Guards the guard: proves the rule rejects a bad pin, so a green result above
/// means the manifest is clean rather than that the check cannot fail.
#[test]
fn the_rule_rejects_a_planted_bad_pin() {
    let root = workspace_root();
    let locked = locked_package_names(&root.join("Cargo.lock"));
    let mut specs = profile_package_specs(&root.join("Cargo.toml"));

    assert!(
        unmatched_specs(&specs, &locked).is_empty(),
        "precondition: the real manifest must be clean before planting"
    );

    // `cosmic-text` is exactly what regressed once: pinned here, gone from the graph.
    assert!(
        !locked.contains("cosmic-text"),
        "cosmic-text is back in the dependency graph; pick another absent name"
    );
    specs.push(("selfdev".to_owned(), "cosmic-text".to_owned()));
    // The wildcard must keep passing, or the rule would flag a legitimate spec.
    specs.push(("selfdev".to_owned(), "*".to_owned()));

    assert_eq!(
        unmatched_specs(&specs, &locked),
        vec!["[profile.selfdev.package.cosmic-text]".to_owned()],
        "the rule must flag the planted pin and only the planted pin"
    );
}

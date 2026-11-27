//! emu never links the Android or iOS device stacks (#325): another device's screen reaches its
//! camera as a `feed:` the simee server serves, so nothing emu builds may depend on
//! `simee-android` or `simee-coresim`, behind a feature or not. Cargo unifies features across the
//! packages one invocation builds (`cargo build -p simee -p simee-emu`, as the release does), so an
//! optional dependency that simee's side enables would reach emu too: this walks every workspace
//! dependency emu's manifest can reach, optional ones included, whatever their features.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The crates emu must never reach.
const DEVICE_STACKS: [&str; 2] = ["simee-android", "simee-coresim"];

/// `[workspace.dependencies]` entries with a path: dependency key -> (package name, directory).
fn workspace_crates(root: &Path) -> BTreeMap<String, (String, PathBuf)> {
    let text = std::fs::read_to_string(root.join("Cargo.toml")).expect("workspace Cargo.toml");
    let mut out = BTreeMap::new();
    let mut in_deps = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_deps = line == "[workspace.dependencies]";
            continue;
        }
        if !in_deps || !line.contains("path =") {
            continue;
        }
        let Some((key, rest)) = line.split_once('=') else { continue };
        let field = |name: &str| {
            let at = rest.find(&format!("{name} = \""))? + name.len() + 4;
            rest[at..].split('"').next().map(str::to_owned)
        };
        let path = field("path").expect("a path");
        let package = field("package").unwrap_or_else(|| key.trim().to_owned());
        out.insert(key.trim().to_owned(), (package, root.join(path)));
    }
    out
}

/// The dependency keys a manifest names in its `[dependencies]` and target dependency tables
/// (not dev- or build-dependencies), optional ones included.
fn dependency_keys(manifest: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(manifest).unwrap_or_else(|e| panic!("{}: {e}", manifest.display()));
    let mut keys = Vec::new();
    let mut in_deps = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_deps = line == "[dependencies]" || (line.starts_with("[target.") && line.ends_with(".dependencies]"));
            continue;
        }
        if in_deps
            && !line.starts_with('#')
            && let Some((key, _)) = line.split_once(['=', '.'])
            && !key.trim().is_empty()
        {
            keys.push(key.trim().to_owned());
        }
    }
    keys
}

/// Every workspace package reachable from the manifest in `dir`, by name, with the path to it.
fn reachable(dir: &Path, crates: &BTreeMap<String, (String, PathBuf)>) -> BTreeMap<String, Vec<String>> {
    let mut seen: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut todo = vec![(dir.to_path_buf(), vec!["simee-emu".to_owned()])];
    let mut visited = BTreeSet::new();
    while let Some((dir, path)) = todo.pop() {
        if !visited.insert(dir.clone()) {
            continue;
        }
        for key in dependency_keys(&dir.join("Cargo.toml")) {
            let Some((package, at)) = crates.get(&key) else { continue };
            let mut via = path.clone();
            via.push(package.clone());
            seen.entry(package.clone()).or_insert_with(|| via.clone());
            todo.push((at.clone(), via));
        }
    }
    seen
}

#[test]
fn emu_reaches_neither_device_stack_even_through_optional_dependencies() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let emu = root.join("apps/emu");
    if !emu.join("Cargo.toml").is_file() {
        eprintln!("skipped: not in the simee workspace, so apps/emu isn't here");
        return;
    }
    let crates = workspace_crates(&root);
    assert!(crates.values().any(|(p, _)| p == "simee-android"), "the walk sees the workspace's crates: {crates:?}");
    let seen = reachable(&emu, &crates);
    assert!(seen.contains_key("simee-camera") && seen.contains_key("simee-emu-vmm"), "the walk follows emu's dependencies: {seen:?}");
    for stack in DEVICE_STACKS {
        assert!(!seen.contains_key(stack), "emu reaches {stack}: {}", seen[stack].join(" -> "));
    }
    // simee itself does link them: the walk would see it if emu did.
    let simee = reachable(&root.join("apps/simee"), &crates);
    assert!(DEVICE_STACKS.iter().all(|s| simee.contains_key(*s)), "{simee:?}");
}

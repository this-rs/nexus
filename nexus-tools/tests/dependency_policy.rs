//! Dependency policy of `nexus-tools` (N17, docs/agent-tools-parity.md §4): everything it
//! ships must compile with the Rust toolchain alone.
//!
//! The rule, checked on the crate's real dependency closure (normal and build edges):
//!
//! - no crate that links a native library (`links = "…"` in its manifest);
//! - no C/C++ toolchain driver in the closure (`cc`, `cmake`, `bindgen`, `pkg-config`,
//!   `vcpkg`, `cxx-build`, …): they are how a crate smuggles in a compiler requirement;
//! - no `-sys` crate, except the platform-binding ones listed in [`PLATFORM_BINDINGS`],
//!   which only declare OS symbols and build nothing.
//!
//! A build script alone is fine (`serde`, `libc` have one that only emits `cfg`s): what
//! matters is what it can compile. When this test fails, do not widen the lists to make it
//! pass: either drop the dependency or amend the policy in the document, with the user.

use std::collections::{BTreeSet, HashMap};
use std::process::Command;

use serde_json::{Value, json};

/// Crates whose job is to drive a native toolchain.
const NATIVE_TOOLCHAIN: &[&str] = &[
    "cc",
    "cmake",
    "bindgen",
    "pkg-config",
    "vcpkg",
    "cxx-build",
    "cxx",
    "zig",
    "nasm-rs",
    "meson",
    "autotools",
];

/// `-sys` crates that only declare OS entry points and compile nothing.
const PLATFORM_BINDINGS: &[&str] = &[
    "windows-sys",
    "core-foundation-sys",
    "system-configuration-sys",
];

/// Why each of them is accepted (kept next to the list so a reviewer sees the claim).
#[test]
fn every_platform_binding_is_justified() {
    for name in PLATFORM_BINDINGS {
        assert!(
            name.ends_with("-sys"),
            "{name} is not a -sys crate: it does not belong in this list"
        );
    }
}

/// What breaks the policy in a `cargo metadata` document, for the closure of `root`.
fn violations(metadata: &Value, root: &str) -> Vec<String> {
    let packages: HashMap<&str, &Value> = metadata["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .map(|p| (p["id"].as_str().expect("id"), p))
        .collect();
    let nodes: HashMap<&str, &Value> = metadata["resolve"]["nodes"]
        .as_array()
        .expect("resolve nodes")
        .iter()
        .map(|n| (n["id"].as_str().expect("id"), n))
        .collect();
    let root_id = packages
        .iter()
        .find(|(_, p)| p["name"] == root)
        .map(|(id, _)| *id)
        .unwrap_or_else(|| panic!("{root} not in the metadata"));

    // Normal and build edges: a dev-dependency never ships. Build edges count because a
    // build script is where a C compiler gets invoked.
    let mut seen = BTreeSet::new();
    let mut stack = vec![root_id];
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        for dep in nodes[id]["deps"].as_array().into_iter().flatten() {
            let ships =
                dep["dep_kinds"].as_array().into_iter().flatten().any(|k| {
                    (k["kind"].is_null() || k["kind"] == "build") && !wasm_only(&k["target"])
                });
            if ships {
                stack.push(dep["pkg"].as_str().expect("pkg"));
            }
        }
    }

    let mut found = Vec::new();
    for id in seen {
        let package = packages[id];
        let name = package["name"].as_str().expect("name");
        if let Some(links) = package["links"].as_str() {
            found.push(format!("{name} links the native library `{links}`"));
        }
        if NATIVE_TOOLCHAIN.contains(&name) {
            found.push(format!("{name} drives a native toolchain"));
        }
        if name.ends_with("-sys") && !PLATFORM_BINDINGS.contains(&name) {
            found.push(format!(
                "{name} is a -sys crate outside the allowed bindings"
            ));
        }
    }
    found
}

/// A dependency that exists only for WebAssembly targets (`cfg(target_arch = "wasm32")`…) is
/// not compiled on any platform this tool supports (unix, windows). A negated condition
/// (`not(…wasm…)`) is the opposite and does count.
fn wasm_only(target: &Value) -> bool {
    target.as_str().is_some_and(|cfg| {
        (cfg.contains("wasm32") || cfg.contains("wasm64") || cfg.contains("\"wasm\""))
            && !cfg.contains("not(")
    })
}

fn package(name: &str, links: Option<&str>) -> Value {
    json!({"id": name, "name": name, "links": links})
}

fn node(name: &str, deps: &[(&str, Option<&str>)]) -> Value {
    json!({"id": name, "deps": deps.iter().map(|(d, kind)| json!({
        "pkg": d, "dep_kinds": [{"kind": kind, "target": null}]
    })).collect::<Vec<_>>()})
}

/// A node whose only edge is restricted to the platforms matching `target`.
fn node_for_target(name: &str, dep: &str, target: &str) -> Value {
    json!({"id": name, "deps": [{"pkg": dep, "dep_kinds": [{"kind": null, "target": target}]}]})
}

fn synthetic(packages: &[Value], nodes: &[Value]) -> Value {
    json!({"packages": packages, "resolve": {"nodes": nodes}})
}

// ---------------------------------------------------------------------------
// The check can fail (red-first: these are what a real violation looks like)
// ---------------------------------------------------------------------------

#[test]
fn a_native_library_in_the_closure_is_refused() {
    let meta = synthetic(
        &[
            package("nexus-tools", None),
            package("zlib-thing", Some("z")),
        ],
        &[
            node("nexus-tools", &[("zlib-thing", None)]),
            node("zlib-thing", &[]),
        ],
    );
    let found = violations(&meta, "nexus-tools");
    assert_eq!(found, ["zlib-thing links the native library `z`"]);
}

#[test]
fn a_c_compiler_reached_through_a_build_dependency_is_refused() {
    let meta = synthetic(
        &[
            package("nexus-tools", None),
            package("fast-hash", None),
            package("cc", None),
        ],
        &[
            node("nexus-tools", &[("fast-hash", None)]),
            node("fast-hash", &[("cc", Some("build"))]),
            node("cc", &[]),
        ],
    );
    assert_eq!(
        violations(&meta, "nexus-tools"),
        ["cc drives a native toolchain"]
    );
}

#[test]
fn an_unlisted_sys_crate_is_refused_but_a_platform_binding_is_not() {
    let meta = synthetic(
        &[
            package("nexus-tools", None),
            package("openssl-sys", None),
            package("windows-sys", None),
        ],
        &[
            node(
                "nexus-tools",
                &[("openssl-sys", None), ("windows-sys", None)],
            ),
            node("openssl-sys", &[]),
            node("windows-sys", &[]),
        ],
    );
    assert_eq!(
        violations(&meta, "nexus-tools"),
        ["openssl-sys is a -sys crate outside the allowed bindings"]
    );
}

#[test]
fn a_dependency_only_for_webassembly_is_not_judged_but_other_platforms_are() {
    let wasm = synthetic(
        &[package("nexus-tools", None), package("js-sys", None)],
        &[
            node_for_target(
                "nexus-tools",
                "js-sys",
                "cfg(all(any(target_arch = \"wasm32\", target_arch = \"wasm64\"), target_os = \"unknown\"))",
            ),
            node("js-sys", &[]),
        ],
    );
    assert!(
        violations(&wasm, "nexus-tools").is_empty(),
        "wasm-only edges are not compiled here"
    );
    // The same crate behind a Windows-only edge IS compiled for Windows users.
    let windows = synthetic(
        &[package("nexus-tools", None), package("js-sys", None)],
        &[
            node_for_target("nexus-tools", "js-sys", "cfg(windows)"),
            node("js-sys", &[]),
        ],
    );
    assert_eq!(
        violations(&windows, "nexus-tools"),
        ["js-sys is a -sys crate outside the allowed bindings"]
    );
    // `not(wasm)` means every other platform: it counts.
    let not_wasm = synthetic(
        &[package("nexus-tools", None), package("js-sys", None)],
        &[
            node_for_target(
                "nexus-tools",
                "js-sys",
                "cfg(not(target_arch = \"wasm32\"))",
            ),
            node("js-sys", &[]),
        ],
    );
    assert_eq!(violations(&not_wasm, "nexus-tools").len(), 1);
}

#[test]
fn a_dev_dependency_does_not_ship_and_is_not_judged() {
    let meta = synthetic(
        &[package("nexus-tools", None), package("cc", None)],
        &[node("nexus-tools", &[("cc", Some("dev"))]), node("cc", &[])],
    );
    assert!(violations(&meta, "nexus-tools").is_empty());
}

// ---------------------------------------------------------------------------
// The real closure
// ---------------------------------------------------------------------------

fn closure_violations(features: &[&str]) -> Vec<String> {
    let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["metadata", "--format-version", "1"])
        .args(features.iter().flat_map(|f| ["--features", f]))
        .arg("--manifest-path")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .output()
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: Value = serde_json::from_slice(&output.stdout).expect("metadata is JSON");
    violations(&metadata, "nexus-tools")
}

/// The exceptions the `tls` feature brings, and nothing else: `ring` compiles C and assembly
/// (decision 91083af7). A second entry here is a new dependency that needs its own decision.
const TLS_EXCEPTIONS: &[&str] = &[
    "ring links its native library",
    "cc drives a native toolchain (ring's build dependency)",
];

#[test]
fn with_tls_the_only_exceptions_are_the_documented_ones() {
    let found = closure_violations(&["tls"]);
    // Compare by crate name so a patch release of ring does not break the test.
    let names: Vec<String> = found
        .iter()
        .map(|v| v.split_whitespace().next().unwrap_or_default().to_owned())
        .collect();
    let allowed: Vec<String> = TLS_EXCEPTIONS
        .iter()
        .map(|v| v.split_whitespace().next().unwrap_or_default().to_owned())
        .collect();
    for name in &names {
        assert!(
            allowed.contains(name),
            "`{name}` is a new non-Rust dependency behind `tls`: {found:?}"
        );
    }
    assert!(
        !names.is_empty(),
        "the exception list is stale: ring no longer shows up"
    );
}

#[test]
fn nexus_tools_compiles_with_the_rust_toolchain_alone() {
    let found = closure_violations(&[]);
    assert!(
        found.is_empty(),
        "nexus-tools must compile with Rust alone (docs/agent-tools-parity.md §4):\n  {}",
        found.join("\n  ")
    );
}

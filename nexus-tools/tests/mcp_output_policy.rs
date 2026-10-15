//! The ONE output policy (GROS-1): a tool result too big to send to the model is persisted and
//! previewed **the same way** by `Bash` and by the native harness's MCP client. The two crates
//! cannot share a constant (the SDK does not depend on the server), so this test reads both
//! sources and fails the day one moves without the other: two thresholds would mean the same
//! content behaving differently because of the tool that produced it.

/// The value of a `usize` constant of Rust source, read by name.
fn const_usize(source: &str, name: &str) -> usize {
    let needle = format!("const {name}: usize =");
    let at = source
        .find(&needle)
        .unwrap_or_else(|| panic!("no `{needle}` in the source"));
    let rest = &source[at + needle.len()..];
    let digits: String = rest
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '_')
        .collect();
    digits
        .replace('_', "")
        .parse()
        .unwrap_or_else(|_| panic!("`{name}` is not a plain number: {digits:?}"))
}

/// The source of a file of the workspace, from this crate's directory.
fn source(relative: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

#[test]
fn the_native_harness_applies_bashs_threshold_and_bashs_preview() {
    let bash_mod = source("src/shell/mod.rs");
    let bash = source("src/shell/bash.rs");
    let sdk = source("../claude-code-sdk-rs/src/providers/native/mcp.rs");

    assert_eq!(
        const_usize(&bash_mod, "INLINE_OUTPUT_LIMIT"),
        const_usize(&sdk, "INLINE_OUTPUT_LIMIT"),
        "the MCP client must keep Bash's threshold, not a second one"
    );
    assert_eq!(
        const_usize(&bash, "PREVIEW_BYTES"),
        const_usize(&sdk, "PREVIEW_BYTES"),
        "the MCP client must keep Bash's preview, not a second one"
    );
    assert_eq!(const_usize(&bash_mod, "INLINE_OUTPUT_LIMIT"), 30_000);
    assert_eq!(const_usize(&bash, "PREVIEW_BYTES"), 2_000);

    // Same tag, same line: what `Read` opens is what both of them name.
    for marker in [
        "<persisted-output>",
        "Preview (first 2KB):",
        "</persisted-output>",
    ] {
        assert!(bash.contains(marker), "Bash no longer writes `{marker}`");
        assert!(
            sdk.contains(marker),
            "the MCP client no longer writes `{marker}`"
        );
    }
}

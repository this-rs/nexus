# Model Usage Guide for Examples

> [!NOTE]
> **Model ids are no longer written in this document.** The catalogue is resolved at
> runtime by `ModelRegistry` (`claude-code-api/src/core/model_registry.rs`): it fetches
> `GET https://api.anthropic.com/v1/models` when `ANTHROPIC_API_KEY` is set, and falls
> back to `ClaudeModel::all()` otherwise. The version-specific ids that used to appear
> below had all been retired, so they were replaced by the aliases `opus`, `sonnet` and
> `haiku`. Ask the server for the exact ids:
> `curl -s http://localhost:8080/v1/models | jq '.data[].id'`.
> `cargo test -p claude-code-api --test docs_model_drift` keeps this true.


## Important Update (2025)

All examples have been updated to use the correct model names based on testing results.

## ✅ Use These Model Names

```rust
// RECOMMENDED - Use aliases for automatic latest version
.model("sonnet")   // Latest Sonnet (currently Sonnet 4)
.model("opus")     // Latest Opus (currently Opus 4.1)

// Or use full names for specific versions
.model("opus")    // Opus 4.1
.model("sonnet")    // Sonnet 4
```

## ❌ DO NOT Use These (They Return 404 Errors)

```rust
.model("opus-4.1")   // ❌ NOT SUPPORTED
.model("sonnet-4")   // ❌ NOT SUPPORTED
.model("opus-4")     // ❌ NOT SUPPORTED
```

## Default Model in Examples

All examples now use `"sonnet"` as the default model for optimal performance.

## Quick Test

Run this to verify which models work on your system:
```bash
cargo run --example simple_model_test
```

## Example Usage

```rust
use nexus_claude::{query, ClaudeCodeOptions, PermissionMode, Result};

#[tokio::main]
async fn main() -> Result<()> {
    // Best practice: Use the alias
    let options = ClaudeCodeOptions::builder()
        .model("sonnet")  // Recommended
        .permission_mode(PermissionMode::Plan)  // v0.1.7 feature
        .build();
    
    let mut messages = query("Hello, Claude!", Some(options)).await?;
    // ... process messages
    
    Ok(())
}
```

## Model Selection Strategy

1. **For most tasks**: Use `"sonnet"` (balanced performance)
2. **For complex reasoning**: Use `"opus"` (most capable)
3. **For fastest response**: Use `"sonnet"` or older models
4. **For production**: Always implement fallback logic

## Fallback Pattern

```rust
async fn query_with_fallback(prompt: &str) -> Result<()> {
    let models = vec!["opus", "sonnet"];  // Try opus first, fallback to sonnet
    
    for model in models {
        let options = ClaudeCodeOptions::builder()
            .model(model)
            .build();
        
        match query(prompt, Some(options)).await {
            Ok(messages) => {
                // Process messages
                return Ok(());
            }
            Err(_) => continue,  // Try next model
        }
    }
    
    Err("All models failed".into())
}
```
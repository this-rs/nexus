# Claude Models Guide (2025)

> [!NOTE]
> **Model ids are no longer written in this document.** The catalogue is resolved at
> runtime by `ModelRegistry` (`claude-code-api/src/core/model_registry.rs`): it fetches
> `GET https://api.anthropic.com/v1/models` when `ANTHROPIC_API_KEY` is set, and falls
> back to `ClaudeModel::all()` otherwise. The version-specific ids that used to appear
> below had all been retired, so they were replaced by the aliases `opus`, `sonnet` and
> `haiku`. Ask the server for the exact ids:
> `curl -s http://localhost:8080/v1/models | jq '.data[].id'`.
> `cargo test -p claude-code-api --test docs_model_drift` keeps this true.


This guide provides comprehensive information about using different Claude models with the cc-sdk.

## Available Models (as of December 2025)

### Opus 4.5 - Most Capable ⭐ NEW (November 2025)
The newest flagship model released on November 24, 2025. Industry-leading performance in coding, agents, and computer use.

**Model identifiers:**
- `"opus"` - Full model name (recommended)
- `"opus"` - General alias (uses latest Opus)

**Key features:**
- 🏆 SWE-bench Verified: **80.9%** (industry-leading)
- 🖥️ OSWorld (computer use): **66.3%** (best in class)
- 💰 Pricing: $5/MTok input, $25/MTok output (cheaper than previous Opus)
- 📝 Context: 200K tokens, Output: 64K tokens
- 🧠 Hybrid reasoning: instant responses or extended thinking

**Example usage:**
```rust
use nexus_claude::{query, ClaudeCodeOptions, Result};
use futures::StreamExt;

#[tokio::main]
async fn main() -> Result<()> {
    let options = ClaudeCodeOptions::builder()
        .model("opus")  // Use latest Opus 4.5
        .max_thinking_tokens(16000)  // Extended thinking supported
        .build();

    let mut messages = query(
        "Analyze this complex algorithm and suggest optimizations",
        Some(options)
    ).await?;

    while let Some(msg) = messages.next().await {
        println!("{:?}", msg?);
    }

    Ok(())
}
```

### Sonnet 4.5 - Balanced Performance
Released in September 2025, excellent balance of capability, speed, and cost.

**Model identifiers:**
- `"sonnet"` - Full model name (recommended)
- `"sonnet"` - General alias (may use latest Sonnet)

**Example usage:**
```rust
use nexus_claude::{query, ClaudeCodeOptions, Result};
use futures::StreamExt;

#[tokio::main]
async fn main() -> Result<()> {
    let options = ClaudeCodeOptions::builder()
        .model("sonnet")  // Use Sonnet 4.5
        .build();

    let mut messages = query(
        "Explain async/await in Rust",
        Some(options)
    ).await?;

    while let Some(msg) = messages.next().await {
        println!("{:?}", msg?);
    }

    Ok(())
}
```

### Sonnet 4 - Cost-Effective
Great balance between capability and cost, ideal for most applications.

**Model identifiers:**
- `"sonnet"` - Full model name for specific version

**Example usage:**
```rust
use nexus_claude::{InteractiveClient, ClaudeCodeOptions, Result};

#[tokio::main]
async fn main() -> Result<()> {
    let options = ClaudeCodeOptions::builder()
        .model("sonnet")
        .permission_mode(nexus_claude::PermissionMode::AcceptEdits)
        .build();

    let mut client = InteractiveClient::new(options)?;
    client.connect().await?;

    let messages = client.send_and_receive(
        "Write a REST API in Rust".to_string()
    ).await?;

    // Process responses...
    client.disconnect().await?;
    Ok(())
}
```

### Previous Generation Models

#### Claude 3.5 Sonnet
- Model ID: `"sonnet"`
- Good for general tasks, previous generation

#### Claude 3.5 Haiku
- Model ID: `"haiku"`
- Fastest response times, suitable for simple tasks

## Choosing the Right Model

### Use Opus 4.5 when you need:
- Complex reasoning and analysis
- Advanced coding tasks (SWE-bench leading)
- Agent and computer use workflows
- Creative writing and content generation
- Multi-step problem solving
- Maximum capability

### Use Sonnet 4.5 when you need:
- Balanced performance and speed
- General programming assistance
- Interactive conversations
- Most day-to-day tasks
- Cost-effective powerful assistance

### Use Haiku when you need:
- Fast responses
- Simple queries
- High-volume processing
- Minimal latency

## Model Features Comparison

| Feature | Opus 4.5 | Sonnet 4.5 | Sonnet 4 | Haiku 3.5 |
|---------|----------|------------|----------|-----------|
| Reasoning | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐⭐ |
| Speed | ⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐⭐⭐⭐ |
| Coding | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐⭐ |
| Agents | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐⭐ |
| Context Length | 200K | 200K | High | Standard |
| Output Tokens | 64K | 64K | 8K | 4K |
| **Recommended For** | **Complex/Coding** | Most tasks | General use | Simple/Fast |

## Advanced Configuration Examples

### Using Extra Arguments with Models
```rust
use nexus_claude::{ClaudeCodeOptions, PermissionMode};
use std::collections::HashMap;

let mut extra_args = HashMap::new();
extra_args.insert("temperature".to_string(), Some("0.7".to_string()));
extra_args.insert("verbose".to_string(), None);

let options = ClaudeCodeOptions::builder()
    .model("opus")  // Latest Opus 4.5
    .permission_mode(PermissionMode::Plan)
    .extra_args(extra_args)
    .max_thinking_tokens(16000)
    .build();
```

### Interactive Session with Model Selection
```rust
use nexus_claude::{InteractiveClient, ClaudeCodeOptions, Result};

async fn create_client_with_model(model: &str) -> Result<InteractiveClient> {
    let options = ClaudeCodeOptions::builder()
        .model(model)
        .system_prompt("You are an expert Rust developer")
        .build();

    Ok(InteractiveClient::new(options)?)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Try Opus 4.5 first
    let mut client = create_client_with_model("opus").await?;

    // Fallback to Sonnet 4.5 if needed
    if client.connect().await.is_err() {
        println!("Opus 4.5 unavailable, falling back to Sonnet 4.5");
        client = create_client_with_model("sonnet").await?;
        client.connect().await?;
    }

    // Use the client...
    Ok(())
}
```

### Using fallback_model (v0.4.0+)
```rust
use nexus_claude::{ClaudeCodeOptions, Result};

let options = ClaudeCodeOptions::builder()
    .model("opus")
    .fallback_model("sonnet")  // Auto-fallback
    .build();
```

## Checking Model Availability

```rust
use nexus_claude::{query, ClaudeCodeOptions, Result};
use futures::StreamExt;

async fn test_model(model_name: &str) -> bool {
    let options = ClaudeCodeOptions::builder()
        .model(model_name)
        .max_turns(1)
        .build();

    match query("Say 'OK'", Some(options)).await {
        Ok(mut stream) => {
            while let Some(msg) = stream.next().await {
                if msg.is_ok() {
                    return true;
                }
            }
            false
        }
        Err(_) => false
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let models = vec![
        "opus",
        "sonnet",
        "opus",
        "sonnet",
        "haiku"
    ];

    for model in models {
        if test_model(model).await {
            println!("✓ {} is available", model);
        } else {
            println!("✗ {} is not available", model);
        }
    }

    Ok(())
}
```

## Error Handling for Invalid Models

```rust
use nexus_claude::{query, ClaudeCodeOptions, SdkError, Result};

async fn safe_query_with_fallback(prompt: &str) -> Result<()> {
    // Try with preferred model
    let result = query_with_model(prompt, "opus").await;

    match result {
        Ok(_) => Ok(()),
        Err(SdkError::CliError { message, .. }) if message.contains("Invalid model") => {
            println!("Model not available, trying fallback...");
            query_with_model(prompt, "sonnet").await
        }
        Err(e) => Err(e)
    }
}

async fn query_with_model(prompt: &str, model: &str) -> Result<()> {
    let options = ClaudeCodeOptions::builder()
        .model(model)
        .build();

    let mut messages = query(prompt, Some(options)).await?;
    // Process messages...
    Ok(())
}
```

## Tips for Model Usage

1. **Always specify a model** - Don't rely on defaults as they may change
2. **Use aliases for flexibility** - `"opus"` and `"sonnet"` automatically use the latest versions
3. **Use fallback_model** - v0.4.0 supports automatic fallback when primary model unavailable
4. **Consider cost vs performance** - Opus 4.5 is most capable ($5/$25 per MTok)
5. **Test with different models** - Performance can vary based on task type

## Environment Variables

You can also set the default model via environment variables:

```bash
export CLAUDE_MODEL="opus"
```

Then in your code:
```rust
let model = std::env::var("CLAUDE_MODEL")
    .unwrap_or_else(|_| "sonnet".to_string());
let options = ClaudeCodeOptions::builder()
    .model(model)
    .build();
```

## Version History

- **2025-11**: Opus 4.5 released (`opus`) ⭐ **Latest**
- **2025-09**: Sonnet 4.5 released (`sonnet`)
- **2025-05**: Sonnet 4 released (`sonnet`)
- **2024-10**: Claude 3.5 series (Sonnet, Haiku)

## See Also

- [README.md](../README.md) - Getting started guide
- [API Documentation](https://docs.rs/cc-sdk) - Full API reference
- [Examples](../examples/) - More code examples
# Claude Sonnet 4.5 Quick Start Guide

> [!NOTE]
> **Model ids are no longer written in this document.** The catalogue is resolved at
> runtime by `ModelRegistry` (`claude-code-api/src/core/model_registry.rs`): it fetches
> `GET https://api.anthropic.com/v1/models` when `ANTHROPIC_API_KEY` is set, and falls
> back to `ClaudeModel::all()` otherwise. The version-specific ids that used to appear
> below had all been retired, so they were replaced by the aliases `opus`, `sonnet` and
> `haiku`. Ask the server for the exact ids:
> `curl -s http://localhost:8080/v1/models | jq '.data[].id'`.
> `cargo test -p claude-code-api --test docs_model_drift` keeps this true.


Claude Sonnet 4.5 (`sonnet`) is the latest model released in September 2025, offering the best balance of performance, speed, and cost-effectiveness.

## Why Choose Sonnet 4.5?

- ⚡ **Fast**: Faster response times than Opus
- 🎯 **Balanced**: Near-Opus quality at 1/3 the cost
- 🆕 **Latest**: Most up-to-date model capabilities
- 💰 **Cost-effective**: ~5x Haiku cost, but much more capable

## Quick Start

### 1. Simple Query

```rust
use nexus_claude::{query, ClaudeCodeOptions, Result};
use futures::StreamExt;

#[tokio::main]
async fn main() -> Result<()> {
    let options = ClaudeCodeOptions::builder()
        .model("sonnet")
        .build();

    let mut messages = query("Explain Rust borrowing", Some(options)).await?;

    while let Some(msg) = messages.next().await {
        println!("{:?}", msg?);
    }

    Ok(())
}
```

### 2. Using Model Recommendation Helper

```rust
use nexus_claude::model_recommendation::latest_sonnet;

let options = ClaudeCodeOptions::builder()
    .model(latest_sonnet())  // Returns "sonnet"
    .build();
```

### 3. Interactive Session

```rust
use nexus_claude::{InteractiveClient, ClaudeCodeOptions, Result};

#[tokio::main]
async fn main() -> Result<()> {
    let options = ClaudeCodeOptions::builder()
        .model("sonnet")
        .system_prompt("You are a helpful Rust expert")
        .build();

    let mut client = InteractiveClient::new(options)?;
    client.connect().await?;

    // First query
    let messages = client.send_and_receive(
        "Create a simple web server".to_string()
    ).await?;

    // Follow-up query with context
    let messages = client.send_and_receive(
        "Add error handling".to_string()
    ).await?;

    client.disconnect().await?;
    Ok(())
}
```

## Migration from Older Models

### From Sonnet 4

```rust
// Before (Sonnet 4)
.model("sonnet")

// After (Sonnet 4.5 - Latest)
.model("sonnet")
// Or simply use the helper
.model(nexus_claude::model_recommendation::latest_sonnet())
```

### Using the Recommendation System

```rust
use nexus_claude::model_recommendation::ModelRecommendation;

let recommender = ModelRecommendation::default();

// Sonnet 4.5 is recommended for these task types:
recommender.suggest("balanced");   // → "sonnet"
recommender.suggest("general");    // → "sonnet"
recommender.suggest("latest");     // → "sonnet"
recommender.suggest("standard");   // → "sonnet"
```

## Advanced Configuration

### Token Optimization

```rust
let options = ClaudeCodeOptions::builder()
    .model("sonnet")
    .max_thinking_tokens(8000)     // Sonnet 4.5 supports extended thinking
    .max_output_tokens(4000)        // Control response length
    .max_turns(5)                   // Limit conversation length
    .build();
```

### With Permission Management

```rust
use nexus_claude::PermissionMode;

let options = ClaudeCodeOptions::builder()
    .model("sonnet")
    .permission_mode(PermissionMode::AcceptEdits)
    .allowed_tools(vec![
        "Read".to_string(),
        "Write".to_string(),
        "Bash".to_string(),
    ])
    .build();
```

### Budget Control

```rust
use nexus_claude::token_tracker::BudgetLimit;

let options = ClaudeCodeOptions::builder()
    .model("sonnet")
    .build();

let mut client = ClaudeSDKClient::new(options);

// Set budget: $10 maximum
client.set_budget_limit(
    BudgetLimit::with_cost(10.0),
    Some(|msg| eprintln!("⚠️  Budget warning: {}", msg))
).await;

// ... use client ...

// Check usage
let usage = client.get_usage_stats().await;
println!("Cost: ${:.4}", usage.total_cost_usd);
```

## Model Comparison Table

| Feature | Sonnet 4.5 | Sonnet 4 | Opus 4.1 | Haiku 3.5 |
|---------|-----------|----------|----------|-----------|
| **Cost (relative)** | 5x | 5x | 15x | 1x |
| **Speed** | ⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐⭐ | ⭐⭐⭐⭐⭐ |
| **Quality** | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐⭐⭐⭐ | ⭐⭐⭐ |
| **Recommended For** | Most tasks | General use | Complex tasks | Simple/fast |

## When to Use Sonnet 4.5

✅ **Use Sonnet 4.5 for:**
- General programming assistance
- Code reviews and refactoring
- Interactive coding sessions
- Documentation generation
- API design and implementation
- Most day-to-day development tasks

❌ **Consider alternatives for:**
- Simple text processing → Use Haiku 3.5 (faster, cheaper)
- Extremely complex reasoning → Use Opus 4.1 (more capable)
- High-volume batch processing → Use Haiku 3.5 (cost-effective)

## Example: Complete Application

```rust
use nexus_claude::{ClaudeSDKClient, ClaudeCodeOptions, Message, Result};
use nexus_claude::model_recommendation::latest_sonnet;
use nexus_claude::token_tracker::BudgetLimit;
use futures::StreamExt;

#[tokio::main]
async fn main() -> Result<()> {
    // Configure with Sonnet 4.5
    let options = ClaudeCodeOptions::builder()
        .model(latest_sonnet())
        .system_prompt("You are a helpful coding assistant")
        .max_output_tokens(3000)
        .build();

    let mut client = ClaudeSDKClient::new(options);

    // Set budget
    client.set_budget_limit(
        BudgetLimit::with_cost(5.0),
        Some(|msg| println!("💰 {}", msg))
    ).await;

    // Connect
    client.connect(Some("Help me create a REST API".to_string())).await?;

    // Receive response
    let mut messages = client.receive_messages().await;
    while let Some(msg) = messages.next().await {
        match msg? {
            Message::Assistant { message } => {
                for block in message.content {
                    if let nexus_claude::ContentBlock::Text(text) = block {
                        println!("{}", text.text);
                    }
                }
            }
            Message::Result { .. } => break,
            _ => {}
        }
    }

    // Send follow-up
    client.send_user_message("Add authentication".to_string()).await?;

    // Get usage stats
    let usage = client.get_usage_stats().await;
    println!("\nUsage: {} tokens, ${:.4}", usage.total_tokens(), usage.total_cost_usd);

    client.disconnect().await?;
    Ok(())
}
```

## See Also

- [Full Models Guide](models-guide.md) - Complete model documentation
- [Token Optimization Guide](TOKEN_OPTIMIZATION.md) - Cost reduction strategies
- [Examples](../examples/sonnet_4_5_example.rs) - Comprehensive examples
- [API Documentation](https://docs.rs/cc-sdk) - Full API reference

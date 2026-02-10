//! # Interactive Memory Chat - Claude Code with Persistent Memory & Streaming
//!
//! This example demonstrates the Nexus SDK's persistent memory system using
//! the `InteractiveClient` for stateful conversations with real-time streaming.
//!
//! ## Key Features
//!
//! - **InteractiveClient**: Maintains conversation state across multiple turns
//! - **Streaming Output**: Real-time token-by-token response display
//! - **Persistent Memory**: Stores conversations in Meilisearch for cross-session retrieval
//! - **MCP Support**: Optional Model Context Protocol server integration
//! - **Context Injection**: Automatically injects relevant historical context
//!
//! ## Prerequisites
//!
//! 1. **Meilisearch** running locally:
//!    ```bash
//!    docker run -d -p 7700:7700 getmeili/meilisearch:latest
//!    ```
//!
//! 2. **Claude Code CLI** installed and authenticated
//!
//! ## Usage
//!
//! ```bash
//! # Basic usage
//! cargo run --example interactive_memory_chat --features memory
//!
//! # With verbose mode (shows injected context)
//! cargo run --example interactive_memory_chat --features memory -- --verbose
//!
//! # With custom Meilisearch URL
//! MEILISEARCH_URL=http://localhost:7700 cargo run --example interactive_memory_chat --features memory
//! ```
//!
//! ## Commands
//!
//! - `/help` - Show available commands
//! - `/list` - List available conversations (from memory)
//! - `/resume <id>` - Resume a previous conversation by ID
//! - `/context` - Show current context (cwd, files touched)
//! - `/history` - Show retrieved historical context
//! - `/session` - Show current session conversation
//! - `/clear` - Clear current conversation
//! - `/stats` - Show memory statistics
//! - `/quit` or `/exit` - Exit the chat

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt;
use nexus_claude::{
    ClaudeCodeOptions, ContentBlock, InteractiveClient, Message, PermissionMode, Result,
};

#[cfg(feature = "memory")]
use nexus_claude::memory::{ContextInjector, MemoryConfig, MemoryIntegrationBuilder};

/// Spinner frames for thinking animation
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Chat configuration parsed from CLI args and environment
struct ChatConfig {
    verbose: bool,
    #[cfg_attr(not(feature = "memory"), allow(dead_code))]
    meilisearch_url: String,
    #[allow(dead_code)] // Reserved for future auth support
    meilisearch_key: Option<String>,
    cwd: String,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            verbose: std::env::args().any(|a| a == "--verbose" || a == "-v"),
            meilisearch_url: std::env::var("MEILISEARCH_URL")
                .unwrap_or_else(|_| "http://localhost:7700".to_string()),
            meilisearch_key: std::env::var("MEILISEARCH_KEY").ok(),
            cwd: std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| ".".to_string()),
        }
    }
}

/// Starts a thinking spinner in the background
fn start_spinner(message: &str) -> Arc<AtomicBool> {
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();
    let message = message.to_string();

    tokio::spawn(async move {
        let mut frame = 0;
        while running_clone.load(Ordering::Relaxed) {
            print!(
                "\r\x1b[1;33m{} {}\x1b[0m\x1b[K",
                SPINNER_FRAMES[frame], message
            );
            io::stdout().flush().unwrap();
            frame = (frame + 1) % SPINNER_FRAMES.len();
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        // Clear the spinner line
        print!("\r\x1b[K");
        io::stdout().flush().unwrap();
    });

    running
}

/// Stops the spinner
fn stop_spinner(running: Arc<AtomicBool>) {
    running.store(false, Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(100));
}

fn print_banner() {
    println!(
        "\n\x1b[1;36m╔═══════════════════════════════════════════════════════════════════╗\x1b[0m"
    );
    println!(
        "\x1b[1;36m║\x1b[0m   \x1b[1;33m◈ NEXUS\x1b[0m - Interactive Claude Code with Streaming & Memory     \x1b[1;36m║\x1b[0m"
    );
    println!(
        "\x1b[1;36m╚═══════════════════════════════════════════════════════════════════╝\x1b[0m\n"
    );
}

fn print_help() {
    println!("\n\x1b[1;33mAvailable Commands:\x1b[0m");
    println!("  \x1b[1;32m/help\x1b[0m          - Show this help message");
    println!("  \x1b[1;32m/list\x1b[0m          - List available conversations from memory");
    println!("  \x1b[1;32m/resume <id>\x1b[0m   - Resume a previous conversation by ID");
    println!("  \x1b[1;32m/session\x1b[0m       - Show current session conversation history");
    println!("  \x1b[1;32m/context\x1b[0m       - Show current context (cwd, files touched)");
    println!("  \x1b[1;32m/history\x1b[0m       - Show retrieved historical context (from memory)");
    println!("  \x1b[1;32m/clear\x1b[0m         - Clear current conversation");
    println!("  \x1b[1;32m/stats\x1b[0m         - Show memory statistics");
    println!("  \x1b[1;32m/quit\x1b[0m          - Exit the chat");
    println!();
}

fn print_status(msg: &str, is_ok: bool) {
    let icon = if is_ok {
        "\x1b[1;32m✓\x1b[0m"
    } else {
        "\x1b[1;31m✗\x1b[0m"
    };
    println!("  {} {}", icon, msg);
}

/// Formats a Unix timestamp as a human-readable age string.
#[cfg(feature = "memory")]
fn format_age(timestamp: i64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let diff = now - timestamp;

    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        let mins = diff / 60;
        format!("{}m ago", mins)
    } else if diff < 86400 {
        let hours = diff / 3600;
        format!("{}h ago", hours)
    } else if diff < 604800 {
        let days = diff / 86400;
        if days == 1 {
            "yesterday".to_string()
        } else {
            format!("{}d ago", days)
        }
    } else {
        let weeks = diff / 604800;
        format!("{}w ago", weeks)
    }
}

#[cfg(feature = "memory")]
async fn check_meilisearch(url: &str) -> std::result::Result<(), String> {
    use meilisearch_sdk::client::Client;

    let client = Client::new(url, Option::<&str>::None).map_err(|e| e.to_string())?;
    client.health().await.map_err(|e| e.to_string())?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = ChatConfig::default();

    print_banner();

    println!("\x1b[1;34mInitializing...\x1b[0m");

    // Check Meilisearch connection
    #[cfg(feature = "memory")]
    let memory_available = {
        match check_meilisearch(&config.meilisearch_url).await {
            Ok(_) => {
                print_status(
                    &format!("Meilisearch connected at {}", config.meilisearch_url),
                    true,
                );
                true
            },
            Err(e) => {
                print_status(&format!("Meilisearch unavailable: {}", e), false);
                println!(
                    "  \x1b[33m→ Memory features disabled. Start Meilisearch for persistence.\x1b[0m"
                );
                false
            },
        }
    };

    #[cfg(not(feature = "memory"))]
    let memory_available = {
        print_status(
            "Memory feature not enabled. Compile with --features memory",
            false,
        );
        false
    };

    print_status(&format!("Working directory: {}", config.cwd), true);
    print_status("Using InteractiveClient with streaming", true);

    if config.verbose {
        print_status("Verbose mode enabled", true);
    }

    println!();
    print_help();

    // Conversation history for current session
    let mut conversation_history: Vec<(String, String)> = Vec::new();
    const MAX_HISTORY_TURNS: usize = 10;

    // Initialize memory manager
    #[cfg(feature = "memory")]
    let mut memory_manager = if memory_available {
        Some(
            MemoryIntegrationBuilder::new()
                .enabled(true)
                .cwd(&config.cwd)
                .url(&config.meilisearch_url)
                .min_relevance_score(0.3)
                .max_context_items(5)
                .build(),
        )
    } else {
        None
    };

    #[cfg(feature = "memory")]
    let context_injector = if memory_available {
        match ContextInjector::new(
            MemoryConfig::default()
                .with_url(&config.meilisearch_url)
                .with_enabled(true),
        )
        .await
        {
            Ok(injector) => Some(injector),
            Err(e) => {
                println!(
                    "  \x1b[33mWarning: Could not initialize context injector: {}\x1b[0m",
                    e
                );
                None
            },
        }
    } else {
        None
    };

    // Create InteractiveClient with options
    // Enable include_partial_messages for true token-by-token streaming
    let options = ClaudeCodeOptions::builder()
        .permission_mode(PermissionMode::BypassPermissions)
        .cwd(&config.cwd)
        .include_partial_messages(true) // Enable streaming of partial messages
        .build();

    let mut client = InteractiveClient::new(options)?;

    // Connect to Claude
    let spinner = start_spinner("Connecting to Claude CLI...");
    match client.connect().await {
        Ok(_) => {
            stop_spinner(spinner);
            print_status("Connected to Claude CLI (InteractiveClient)", true);
        },
        Err(e) => {
            stop_spinner(spinner);
            print_status(&format!("Failed to connect: {}", e), false);
            return Err(e);
        },
    }

    println!();

    // Main chat loop
    loop {
        // Print prompt
        print!("\x1b[1;32mYou>\x1b[0m ");
        io::stdout().flush().unwrap();

        // Read input
        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() {
            break;
        }

        let input = input.trim();

        if input.is_empty() {
            continue;
        }

        // Handle commands
        if input.starts_with('/') {
            match input {
                "/help" | "/h" => {
                    print_help();
                    continue;
                },
                "/quit" | "/exit" | "/q" => {
                    println!("\n\x1b[1;33mGoodbye! Your conversation has been saved.\x1b[0m\n");
                    break;
                },
                "/session" | "/s" => {
                    println!(
                        "\n\x1b[1;34mSession History ({} messages):\x1b[0m",
                        conversation_history.len()
                    );
                    if conversation_history.is_empty() {
                        println!("  \x1b[2m(empty)\x1b[0m");
                    } else {
                        for (i, (role, content)) in conversation_history.iter().enumerate() {
                            let role_color = if role == "user" { "32" } else { "34" };
                            let role_label = if role == "user" { "You" } else { "Claude" };
                            let preview = if content.len() > 80 {
                                format!("{}...", &content[..80])
                            } else {
                                content.clone()
                            };
                            println!(
                                "  \x1b[1;2m{}.\x1b[0m \x1b[1;{}m{}>\x1b[0m {}",
                                i + 1,
                                role_color,
                                role_label,
                                preview.replace('\n', " ")
                            );
                        }
                    }
                    println!();
                    continue;
                },
                "/clear" => {
                    conversation_history.clear();

                    #[cfg(feature = "memory")]
                    if let Some(ref mut manager) = memory_manager {
                        let pending = manager.take_pending_messages();
                        if !pending.is_empty()
                            && let Some(ref injector) = context_injector
                        {
                            let _ = injector.store_messages(&pending).await;
                        }
                        *manager = MemoryIntegrationBuilder::new()
                            .enabled(true)
                            .cwd(&config.cwd)
                            .min_relevance_score(0.3)
                            .max_context_items(5)
                            .build();
                    }
                    println!("\x1b[1;33mConversation cleared. Starting fresh.\x1b[0m\n");
                    continue;
                },
                "/context" => {
                    #[cfg(feature = "memory")]
                    if let Some(ref manager) = memory_manager {
                        println!("\n\x1b[1;34mCurrent Context:\x1b[0m");
                        println!("  Conversation ID: {}", manager.conversation_id());
                        println!("  Working Directory: {}", manager.cwd().unwrap_or("(none)"));
                        println!("  Turn Index: {}", manager.turn_index());
                        let ctx = manager.current_context("");
                        if !ctx.files.is_empty() {
                            println!("  Files Touched:");
                            for file in &ctx.files {
                                println!("    - {}", file);
                            }
                        }
                        println!();
                    }
                    #[cfg(not(feature = "memory"))]
                    println!("\x1b[33mMemory not enabled.\x1b[0m\n");
                    continue;
                },
                "/history" => {
                    #[cfg(feature = "memory")]
                    if let Some(ref injector) = context_injector {
                        if let Some(ref manager) = memory_manager {
                            println!("\n\x1b[1;34mRetrieving historical context...\x1b[0m\n");
                            match injector
                                .get_context_prefix(
                                    "recent conversation context",
                                    manager.cwd(),
                                    &manager.current_context("").files,
                                )
                                .await
                            {
                                Ok(Some(ctx)) => {
                                    println!("{}", ctx);
                                },
                                Ok(None) => {
                                    println!(
                                        "\x1b[33mNo relevant historical context found.\x1b[0m\n"
                                    );
                                },
                                Err(e) => {
                                    println!("\x1b[31mError retrieving context: {}\x1b[0m\n", e);
                                },
                            }
                        }
                    } else {
                        println!("\x1b[33mMemory not available.\x1b[0m\n");
                    }
                    #[cfg(not(feature = "memory"))]
                    println!("\x1b[33mMemory not enabled.\x1b[0m\n");
                    continue;
                },
                "/stats" => {
                    #[cfg(feature = "memory")]
                    if let Some(ref manager) = memory_manager {
                        println!("\n\x1b[1;34mMemory Statistics:\x1b[0m");
                        println!("  Memory Enabled: {}", manager.is_enabled());
                        println!("  Current Turn: {}", manager.turn_index());
                        println!("  Is Resumed: {}", manager.is_resumed());
                        println!(
                            "  Min Relevance Score: {}",
                            manager.config().min_relevance_score
                        );
                        println!(
                            "  Max Context Items: {}",
                            manager.config().max_context_items
                        );
                        println!("  Token Budget: {}", manager.config().token_budget);
                        println!("  Client Type: InteractiveClient (stateful)");
                        println!();
                    }
                    #[cfg(not(feature = "memory"))]
                    println!("\x1b[33mMemory not enabled.\x1b[0m\n");
                    continue;
                },
                "/list" => {
                    #[cfg(feature = "memory")]
                    if let Some(ref injector) = context_injector {
                        println!("\n\x1b[1;34m📚 Available Conversations:\x1b[0m\n");
                        match injector.list_conversations(10, 0).await {
                            Ok(conversations) => {
                                if conversations.is_empty() {
                                    println!("  \x1b[2m(no conversations found)\x1b[0m");
                                } else {
                                    for (i, conv) in conversations.iter().enumerate() {
                                        let age = format_age(conv.updated_at);
                                        let cwd_display = conv
                                            .cwd
                                            .as_ref()
                                            .map(|c| {
                                                if c.len() > 30 {
                                                    format!("...{}", &c[c.len() - 27..])
                                                } else {
                                                    c.clone()
                                                }
                                            })
                                            .unwrap_or_else(|| "(no cwd)".to_string());
                                        println!(
                                            "  \x1b[1;33m{}.\x1b[0m \x1b[1;36m{}\x1b[0m",
                                            i + 1,
                                            conv.id
                                        );
                                        println!(
                                            "     {} messages, {} • {}",
                                            conv.message_count, age, cwd_display
                                        );
                                    }
                                }
                            },
                            Err(e) => {
                                println!("\x1b[31mError listing conversations: {}\x1b[0m", e);
                            },
                        }
                        println!();
                    } else {
                        println!("\x1b[33mMemory not available.\x1b[0m\n");
                    }
                    #[cfg(not(feature = "memory"))]
                    println!("\x1b[33mMemory not enabled.\x1b[0m\n");
                    continue;
                },
                cmd if cmd.starts_with("/resume ") => {
                    let conv_id = cmd.strip_prefix("/resume ").unwrap().trim();
                    if conv_id.is_empty() {
                        println!("\x1b[31mUsage: /resume <conversation_id>\x1b[0m\n");
                        continue;
                    }

                    #[cfg(feature = "memory")]
                    if let Some(ref injector) = context_injector {
                        println!(
                            "\n\x1b[1;34m📜 Loading conversation {}...\x1b[0m\n",
                            conv_id
                        );
                        match injector.load_conversation(conv_id, Some(20), None).await {
                            Ok(loaded) => {
                                if loaded.is_empty() {
                                    println!(
                                        "\x1b[33mNo messages found for conversation {}\x1b[0m\n",
                                        conv_id
                                    );
                                    continue;
                                }

                                // Display loaded messages in chronological order
                                println!(
                                    "\x1b[1;32m✓ Loaded {} messages (total: {})\x1b[0m\n",
                                    loaded.len(),
                                    loaded.total_count
                                );

                                for msg in loaded.messages_chronological() {
                                    let role_color = if msg.role == "user" { "32" } else { "34" };
                                    let role_label =
                                        if msg.role == "user" { "You" } else { "Claude" };
                                    let preview = if msg.content.len() > 100 {
                                        format!("{}...", &msg.content[..100])
                                    } else {
                                        msg.content.clone()
                                    };
                                    println!(
                                        "  \x1b[1;{}m{}>\x1b[0m {}",
                                        role_color,
                                        role_label,
                                        preview.replace('\n', " ")
                                    );
                                }
                                println!();

                                // Resume the conversation in the manager
                                if let Some(ref mut manager) = memory_manager {
                                    let next_turn =
                                        loaded.max_turn_index().map(|i| i + 1).unwrap_or(0);
                                    let cwd =
                                        loaded.messages.first().and_then(|m| m.cwd.as_deref());

                                    manager.resume_conversation(conv_id, cwd, next_turn);

                                    // Also update local conversation history for context building
                                    conversation_history.clear();
                                    for msg in loaded.messages_chronological() {
                                        conversation_history
                                            .push((msg.role.clone(), msg.content.clone()));
                                    }

                                    // Trim to max history
                                    while conversation_history.len() > MAX_HISTORY_TURNS * 2 {
                                        conversation_history.remove(0);
                                    }

                                    println!(
                                        "\x1b[1;33m→ Resuming from turn {}. Continue the conversation below.\x1b[0m\n",
                                        next_turn
                                    );
                                }
                            },
                            Err(e) => {
                                println!("\x1b[31mError loading conversation: {}\x1b[0m\n", e);
                            },
                        }
                    } else {
                        println!("\x1b[33mMemory not available.\x1b[0m\n");
                    }
                    #[cfg(not(feature = "memory"))]
                    {
                        let _ = conv_id; // suppress unused warning
                        println!("\x1b[33mMemory not enabled.\x1b[0m\n");
                    }
                    continue;
                },
                _ => {
                    println!(
                        "\x1b[31mUnknown command: {}\x1b[0m. Type /help for available commands.\n",
                        input
                    );
                    continue;
                },
            }
        }

        // Record user message in memory
        #[cfg(feature = "memory")]
        if let Some(ref mut manager) = memory_manager {
            manager.record_user_message(input);
        }

        // Get historical context for injection
        #[cfg(feature = "memory")]
        let context_prefix: Option<String> = if let Some(ref injector) = context_injector {
            if let Some(ref manager) = memory_manager {
                let spinner = start_spinner("Retrieving context...");
                let result = injector
                    .get_context_prefix(input, manager.cwd(), &manager.current_context(input).files)
                    .await;
                stop_spinner(spinner);

                match result {
                    Ok(Some(ctx)) => {
                        if config.verbose {
                            println!("\x1b[1;35m┌─── Injected Context ───\x1b[0m");
                            for line in ctx.lines() {
                                println!("\x1b[1;35m│\x1b[0m {}", line);
                            }
                            println!("\x1b[1;35m└────────────────────────\x1b[0m\n");
                        } else {
                            println!("\x1b[2m📚 Context retrieved ({} chars)\x1b[0m", ctx.len());
                        }
                        Some(ctx)
                    },
                    Ok(None) => None,
                    Err(e) => {
                        if config.verbose {
                            println!("\x1b[33mWarning: Context retrieval error: {}\x1b[0m", e);
                        }
                        None
                    },
                }
            } else {
                None
            }
        } else {
            None
        };

        #[cfg(not(feature = "memory"))]
        let context_prefix: Option<String> = None;

        // Build prompt with context
        let mut prompt_parts = Vec::new();

        if !conversation_history.is_empty() || context_prefix.is_some() {
            prompt_parts.push("<system>You are continuing a conversation. Use the context below to maintain continuity.</system>".to_string());
            prompt_parts.push(String::new());
        }

        if let Some(ref ctx) = context_prefix {
            prompt_parts.push("<historical_context>".to_string());
            prompt_parts.push(ctx.clone());
            prompt_parts.push("</historical_context>".to_string());
            prompt_parts.push(String::new());
        }

        if !conversation_history.is_empty() {
            prompt_parts.push("<current_session>".to_string());
            for (role, content) in &conversation_history {
                let tag = if role == "user" { "human" } else { "assistant" };
                let max_len = if role == "assistant" { 2000 } else { 500 };
                let truncated = if content.len() > max_len {
                    format!("{}...", &content[..max_len])
                } else {
                    content.clone()
                };
                prompt_parts.push(format!("<{}>", tag));
                prompt_parts.push(truncated);
                prompt_parts.push(format!("</{}>", tag));
            }
            prompt_parts.push("</current_session>".to_string());
            prompt_parts.push(String::new());
        }

        prompt_parts.push("<human>".to_string());
        prompt_parts.push(input.to_string());
        prompt_parts.push("</human>".to_string());

        let prompt = prompt_parts.join("\n");

        if config.verbose {
            println!(
                "\n\x1b[1;35m┌─── Full Prompt ({} chars) ───\x1b[0m",
                prompt.len()
            );
            for line in prompt.lines().take(20) {
                println!("\x1b[35m│\x1b[0m {}", line);
            }
            if prompt.lines().count() > 20 {
                println!(
                    "\x1b[35m│\x1b[0m ... ({} more lines)\x1b[0m",
                    prompt.lines().count() - 20
                );
            }
            println!("\x1b[1;35m└────────────────────────────────\x1b[0m\n");
        }

        // Receive response with streaming
        let mut response_text = String::new();
        let mut displayed_len = 0; // Track how much text we've already displayed
        let mut first_token = true;
        let spinner = start_spinner("Thinking...");

        // Use send_and_receive_stream to avoid race condition between send and subscribe
        // This method subscribes to the broadcast BEFORE sending the message
        let stream_result = client.send_and_receive_stream(prompt).await;
        let stream = match stream_result {
            Ok(s) => s,
            Err(e) => {
                stop_spinner(spinner);
                println!("\x1b[31mError sending message: {}\x1b[0m\n", e);
                continue;
            },
        };

        // Pin the stream since async_stream returns a !Unpin type
        let mut stream = std::pin::pin!(stream);

        while let Some(msg_result) = stream.next().await {
            match msg_result {
                Ok(Message::Assistant { message, .. }) => {
                    if first_token {
                        stop_spinner(spinner.clone());
                        print!("\x1b[1;34mClaude>\x1b[0m ");
                        io::stdout().flush().unwrap();
                        first_token = false;
                    }

                    for block in &message.content {
                        match block {
                            ContentBlock::Text(text_content) => {
                                // With include_partial_messages, we receive cumulative text.
                                // Only print the NEW characters (delta) since last update.
                                let full_text = &text_content.text;
                                if full_text.len() > displayed_len {
                                    let delta = &full_text[displayed_len..];
                                    print!("{}", delta);
                                    io::stdout().flush().unwrap();
                                    displayed_len = full_text.len();
                                }
                                // Update response_text with full content for final storage
                                response_text = full_text.clone();
                            },
                            ContentBlock::ToolUse(tool_use) => {
                                // Show tool usage
                                println!("\n\x1b[2m  🔧 Using tool: {}\x1b[0m", tool_use.name);
                                io::stdout().flush().unwrap();

                                // Record tool call for memory context
                                #[cfg(feature = "memory")]
                                if let Some(ref mut mgr) = memory_manager {
                                    mgr.process_tool_call(&tool_use.name, &tool_use.input);
                                }
                            },
                            ContentBlock::ToolResult(tool_result) => {
                                if config.verbose {
                                    let preview = match &tool_result.content {
                                        Some(nexus_claude::ContentValue::Text(text)) => {
                                            if text.len() > 100 {
                                                format!("{}...", &text[..100])
                                            } else {
                                                text.clone()
                                            }
                                        },
                                        Some(nexus_claude::ContentValue::Structured(arr)) => {
                                            format!("[{} items]", arr.len())
                                        },
                                        None => "(no content)".to_string(),
                                    };
                                    println!(
                                        "\n\x1b[2m  📋 Tool result: {}\x1b[0m",
                                        preview.replace('\n', " ")
                                    );
                                }
                            },
                            ContentBlock::Thinking(thinking) => {
                                if config.verbose {
                                    println!(
                                        "\n\x1b[2;3m  💭 {}\x1b[0m",
                                        thinking.thinking.chars().take(100).collect::<String>()
                                    );
                                }
                            },
                        }
                    }
                },
                Ok(Message::StreamEvent {
                    event:
                        nexus_claude::StreamEventData::ContentBlockDelta {
                            delta: nexus_claude::StreamDelta::TextDelta { text },
                            ..
                        },
                    ..
                }) => {
                    // Real-time token streaming via StreamEvent
                    if first_token {
                        stop_spinner(spinner.clone());
                        print!("\x1b[1;34mClaude>\x1b[0m ");
                        io::stdout().flush().unwrap();
                        first_token = false;
                    }
                    print!("{}", text);
                    io::stdout().flush().unwrap();
                    response_text.push_str(&text);
                },
                Ok(Message::StreamEvent { .. }) => {
                    // Other stream events (message_start, content_block_stop, etc.)
                },
                Ok(Message::Result {
                    total_cost_usd,
                    duration_ms,
                    ..
                }) => {
                    if first_token {
                        stop_spinner(spinner.clone());
                    }
                    // Show cost and timing info
                    let cost_str = total_cost_usd
                        .map(|c| format!("💰 ${:.4}", c))
                        .unwrap_or_default();
                    let time_str = format!("⏱ {}ms", duration_ms);
                    if !cost_str.is_empty() || duration_ms > 0 {
                        println!("\n\x1b[2m  {} {}\x1b[0m", cost_str, time_str);
                    }
                    break;
                },
                Err(e) => {
                    if first_token {
                        stop_spinner(spinner.clone());
                    }
                    println!("\n\x1b[31mError: {}\x1b[0m", e);
                    break;
                },
                _ => {},
            }
        }

        println!();

        // Update conversation history
        if !response_text.is_empty() {
            conversation_history.push(("user".to_string(), input.to_string()));
            conversation_history.push(("assistant".to_string(), response_text.clone()));

            while conversation_history.len() > MAX_HISTORY_TURNS * 2 {
                conversation_history.remove(0);
            }
        }

        // Record assistant message in memory
        #[cfg(feature = "memory")]
        if let Some(ref mut manager) = memory_manager
            && !response_text.is_empty()
        {
            manager.record_assistant_message(&response_text);
        }

        // Store messages periodically
        #[cfg(feature = "memory")]
        if let Some(ref mut manager) = memory_manager {
            let pending = manager.take_pending_messages();
            if !pending.is_empty()
                && let Some(ref injector) = context_injector
                && let Err(e) = injector.store_messages(&pending).await
                && config.verbose
            {
                println!("\x1b[33mWarning: Could not store messages: {}\x1b[0m", e);
            }
        }
    }

    // Disconnect client
    let _ = client.disconnect().await;

    // Suppress unused variable warning
    let _ = memory_available;

    Ok(())
}

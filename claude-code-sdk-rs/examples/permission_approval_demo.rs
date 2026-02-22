//! Permission Approval Demo — Interactive permission flow using InteractiveClient
//!
//! This example demonstrates the **SDK control protocol** permission flow:
//! 1. Connect an InteractiveClient in `Default` permission mode (Claude asks for approval)
//! 2. Send a prompt that triggers tool use (e.g., `echo Hello` via Bash)
//! 3. Listen for permission requests on the SDK control channel
//! 4. Display the request to the user and wait for stdin approval (y/n)
//! 5. Send the approval/denial back to the CLI via `send_control_response`
//!
//! This is the same pattern used by the PO Backend (`ChatManager::stream_response`):
//!   - `take_sdk_control_receiver()` to get the raw control channel
//!   - `tokio::select!` between stream messages and control requests
//!   - `send_control_response()` to approve or deny
//!
//! Usage:
//!   cargo run --example permission_approval_demo
//!
//! Requirements:
//!   - Claude CLI must be installed and configured (`claude` in PATH)
//!   - A valid API key / authentication

use futures::StreamExt;
use nexus_claude::{ClaudeCodeOptions, InteractiveClient, Message, PermissionMode, Result};
use std::io::{self, Write};
use std::sync::Arc;
use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging (set RUST_LOG=debug for verbose output)
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "permission_approval_demo=info,nexus_claude=warn"
                    .parse()
                    .unwrap()
            }),
        )
        .init();

    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║     Permission Approval Demo — SDK Control Protocol     ║");
    println!("╚══════════════════════════════════════════════════════════╝\n");

    // Create client in Default mode — Claude will ask for tool permission
    let options = ClaudeCodeOptions::builder()
        .system_prompt(
            "You are a helpful assistant. When asked, use the Bash tool to execute commands.",
        )
        .permission_mode(PermissionMode::Default)
        .build();

    let mut client = InteractiveClient::new(options)?;

    println!("🔌 Connecting to Claude CLI...");
    client.connect().await?;
    println!("✅ Connected!\n");

    // Take the SDK control receiver — this is the channel where permission
    // requests arrive (same pattern as PO Backend's ChatManager)
    let mut sdk_control_rx = client
        .take_sdk_control_receiver()
        .await
        .expect("SDK control receiver should be available");

    // Wrap in Arc<Mutex<>> so we can share between stream and control handler
    // (same pattern as PO Backend's ActiveSession.client)
    let client = Arc::new(Mutex::new(client));

    // Send a prompt that will trigger a tool use requiring permission
    let prompt = "Please run: echo 'Hello from permission demo!'";
    println!("📤 Sending prompt: \"{prompt}\"\n");
    {
        let mut c = client.lock().await;
        c.send_message(prompt.to_string()).await?;
    }

    // Start receiving the message stream.
    //
    // `receive_messages_stream()` takes `&mut self` so it borrows the client.
    // We need to hold the lock for the entire duration of the stream.
    // To handle permission requests concurrently, we send the control response
    // directly through the transport (which is behind its own Arc<Mutex<>>
    // inside the InteractiveClient).
    //
    // Alternative design (used by PO Backend): the InteractiveClient is behind
    // Arc<Mutex<>> and stream_response takes ownership of the lock for the
    // entire streaming phase, sending control responses via a separate code path.
    //
    // For this example, we use a simpler approach: forward stream messages to a
    // channel and process both channels in the main loop.
    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel::<Result<Message>>(100);
    {
        let client = client.clone();
        tokio::spawn(async move {
            // Hold the lock for the entire stream duration
            let mut c = client.lock().await;
            let mut stream = c.receive_messages_stream().await;

            while let Some(result) = stream.next().await {
                if msg_tx.send(result).await.is_err() {
                    break;
                }
            }
            // Lock released here when stream ends
        });
    }

    // Main event loop — listen for BOTH stream messages and control requests
    // using tokio::select!, exactly like PO Backend's stream_response
    let mut done = false;

    while !done {
        tokio::select! {
            // Branch 1: Stream messages from Claude
            msg = msg_rx.recv() => {
                match msg {
                    Some(Ok(message)) => {
                        handle_message(&message);
                        if matches!(message, Message::Result { .. }) {
                            done = true;
                        }
                    }
                    Some(Err(e)) => {
                        eprintln!("❌ Stream error: {e}");
                        done = true;
                    }
                    None => {
                        println!("📭 Stream ended");
                        done = true;
                    }
                }
            }

            // Branch 2: SDK control requests (permission requests from CLI)
            control_msg = sdk_control_rx.recv() => {
                match control_msg {
                    Some(msg) => {
                        let allow = prompt_permission(&msg);
                        let response = serde_json::json!({ "allow": allow });
                        // Wait for the stream task to release the client lock,
                        // then send the control response. In practice, the CLI
                        // pauses the stream while waiting for permission, so the
                        // lock should be available quickly.
                        let mut c = client.lock().await;
                        if let Err(e) = c.send_control_response(response).await {
                            eprintln!("   ⚠️  Failed to send response: {e}");
                        }
                    }
                    None => {
                        println!("📭 Control channel closed");
                        done = true;
                    }
                }
            }
        }
    }

    // Cleanup
    println!("\n🔌 Disconnecting...");
    {
        let mut c = client.lock().await;
        c.disconnect().await?;
    }
    println!("👋 Done!");

    Ok(())
}

/// Handle a stream message from Claude
fn handle_message(message: &Message) {
    match message {
        Message::Assistant { message, .. } => {
            for block in &message.content {
                match block {
                    nexus_claude::ContentBlock::Text(text) => {
                        println!("🤖 Claude: {}", text.text);
                    },
                    nexus_claude::ContentBlock::ToolUse(tool) => {
                        println!("🔧 Tool use: {} (id: {})", tool.name, tool.id);
                        println!(
                            "   Input: {}",
                            serde_json::to_string_pretty(&tool.input).unwrap_or_default()
                        );
                    },
                    nexus_claude::ContentBlock::ToolResult(result) => {
                        println!("📋 Tool result ({})", result.tool_use_id);
                    },
                    nexus_claude::ContentBlock::Thinking(thinking) => {
                        let preview = if thinking.thinking.len() > 80 {
                            format!("{}...", &thinking.thinking[..80])
                        } else {
                            thinking.thinking.clone()
                        };
                        println!("💭 Thinking: {preview}");
                    },
                }
            }
        },
        Message::Result {
            duration_ms,
            total_cost_usd,
            is_error,
            ..
        } => {
            if *is_error {
                println!("❌ Completed with error in {duration_ms}ms");
            } else {
                print!("✅ Completed in {duration_ms}ms");
                if let Some(cost) = total_cost_usd {
                    print!(" (cost: ${cost:.6})");
                }
                println!();
            }
        },
        Message::System { subtype, .. } => {
            println!("⚙️  System: {subtype}");
        },
        _ => {},
    }
}

/// Display a permission request and ask for user approval via stdin.
/// Returns `true` if the user approves.
fn prompt_permission(msg: &serde_json::Value) -> bool {
    let request = msg.get("request").unwrap_or(msg);
    let subtype = request
        .get("subtype")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    if subtype != "can_use_tool" {
        println!("ℹ️  Non-permission control request: {subtype}");
        return true; // auto-allow non-permission requests
    }

    let tool_name = request
        .get("toolName")
        .or_else(|| request.get("tool_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let input = request
        .get("input")
        .cloned()
        .unwrap_or(serde_json::json!({}));

    println!("\n╔══════════════════════════════════════════════════════╗");
    println!("║              🔐 PERMISSION REQUEST                  ║");
    println!("╠══════════════════════════════════════════════════════╣");
    println!("║  Tool:  {:<44}║", tool_name);
    if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
        let display = if cmd.len() > 44 {
            format!("{}...", &cmd[..41])
        } else {
            cmd.to_string()
        };
        println!("║  Cmd:   {:<44}║", display);
    } else {
        let input_str = serde_json::to_string(&input).unwrap_or_default();
        let display = if input_str.len() > 44 {
            format!("{}...", &input_str[..41])
        } else {
            input_str
        };
        println!("║  Input: {:<44}║", display);
    }
    println!("╚══════════════════════════════════════════════════════╝");

    // Ask user for approval
    print!("\n   Allow this tool use? [y/n]: ");
    io::stdout().flush().unwrap();

    let mut response = String::new();
    io::stdin().read_line(&mut response).unwrap();
    let allow = response.trim().to_lowercase().starts_with('y');

    if allow {
        println!("   ✅ Approved\n");
    } else {
        println!("   ❌ Denied\n");
    }

    allow
}

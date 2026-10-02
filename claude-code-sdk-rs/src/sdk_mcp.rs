#![allow(missing_docs)]
//! SDK MCP Server - In-process MCP server implementation
//!
//! This module provides an in-process MCP server that runs directly within your
//! Rust application, eliminating the need for separate processes.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

use crate::errors::{Result, SdkError};

/// Tool input schema definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInputSchema {
    #[serde(rename = "type")]
    pub schema_type: String,
    pub properties: HashMap<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<Vec<String>>,
}

/// Tool definition
#[derive(Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: ToolInputSchema,
    pub handler: Arc<dyn ToolHandler>,
}

impl std::fmt::Debug for ToolDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolDefinition")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("input_schema", &self.input_schema)
            .field("handler", &"<Arc<dyn ToolHandler>>")
            .finish()
    }
}

/// Tool handler trait
#[async_trait]
pub trait ToolHandler: Send + Sync {
    async fn execute(&self, args: Value) -> Result<ToolResult>;
}

/// Tool execution result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: Vec<ToolResultContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

/// Tool result content types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ToolResultContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

/// SDK MCP Server
pub struct SdkMcpServer {
    pub name: String,
    pub version: String,
    pub tools: Vec<ToolDefinition>,
}

impl SdkMcpServer {
    /// Create a new SDK MCP server
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            tools: Vec::new(),
        }
    }

    /// Add a tool to the server
    pub fn add_tool(&mut self, tool: ToolDefinition) {
        self.tools.push(tool);
    }

    /// Handle MCP protocol messages
    pub async fn handle_message(&self, message: Value) -> Result<Value> {
        let method = message
            .get("method")
            .and_then(|m| m.as_str())
            .ok_or_else(|| SdkError::InvalidState {
                message: "Missing method in MCP message".to_string(),
            })?;

        let id = message.get("id");

        match method {
            "initialize" => Ok(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": self.name,
                        "version": self.version
                    }
                }
            })),

            "tools/list" => {
                let tools: Vec<Value> = self
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "name": tool.name,
                            "description": tool.description,
                            "inputSchema": tool.input_schema
                        })
                    })
                    .collect();

                Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": tools
                    }
                }))
            },

            "tools/call" => {
                let params = message
                    .get("params")
                    .ok_or_else(|| SdkError::InvalidState {
                        message: "Missing params in tools/call".to_string(),
                    })?;

                let tool_name = params.get("name").and_then(|n| n.as_str()).ok_or_else(|| {
                    SdkError::InvalidState {
                        message: "Missing tool name in tools/call".to_string(),
                    }
                })?;

                let empty_args = json!({});
                let arguments = params.get("arguments").unwrap_or(&empty_args);

                // Find and execute the tool
                let tool = self
                    .tools
                    .iter()
                    .find(|t| t.name == tool_name)
                    .ok_or_else(|| SdkError::InvalidState {
                        message: format!("Tool not found: {tool_name}"),
                    })?;

                let result = tool.handler.execute(arguments.clone()).await?;

                Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": result.content,
                        "isError": result.is_error
                    }
                }))
            },

            "notifications/initialized" => {
                // Acknowledge initialization notification
                Ok(json!({
                    "jsonrpc": "2.0",
                    "result": {}
                }))
            },

            _ => Ok(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("Method '{}' not found", method)
                }
            })),
        }
    }
}

impl SdkMcpServer {
    /// Convert to McpServerConfig
    pub fn to_config(self) -> crate::types::McpServerConfig {
        use std::sync::Arc;
        crate::types::McpServerConfig::Sdk {
            name: self.name.clone(),
            instance: Arc::new(self),
        }
    }
}

/// Builder for creating SDK MCP servers
pub struct SdkMcpServerBuilder {
    name: String,
    version: String,
    tools: Vec<ToolDefinition>,
}

impl SdkMcpServerBuilder {
    /// Create a new builder
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: "1.0.0".to_string(),
            tools: Vec::new(),
        }
    }

    /// Set server version
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// Add a tool
    pub fn tool(mut self, tool: ToolDefinition) -> Self {
        self.tools.push(tool);
        self
    }

    /// Build the server
    pub fn build(self) -> SdkMcpServer {
        SdkMcpServer {
            name: self.name,
            version: self.version,
            tools: self.tools,
        }
    }
}

/// Helper function to create a simple text-based tool
pub fn create_simple_tool<F, Fut>(
    name: impl Into<String>,
    description: impl Into<String>,
    schema: ToolInputSchema,
    handler: F,
) -> ToolDefinition
where
    F: Fn(Value) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<String>> + Send + 'static,
{
    struct SimpleHandler<F, Fut>
    where
        F: Fn(Value) -> Fut + Send + Sync,
        Fut: std::future::Future<Output = Result<String>> + Send,
    {
        func: F,
    }

    #[async_trait]
    impl<F, Fut> ToolHandler for SimpleHandler<F, Fut>
    where
        F: Fn(Value) -> Fut + Send + Sync,
        Fut: std::future::Future<Output = Result<String>> + Send,
    {
        async fn execute(&self, args: Value) -> Result<ToolResult> {
            let text = (self.func)(args).await?;
            Ok(ToolResult {
                content: vec![ToolResultContent::Text { text }],
                is_error: None,
            })
        }
    }

    ToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema: schema,
        handler: Arc::new(SimpleHandler { func: handler }),
    }
}

/// Macro to define a tool with a simple syntax
#[macro_export]
macro_rules! tool {
    ($name:expr, $desc:expr, $schema:expr, $handler:expr) => {
        $crate::sdk_mcp::create_simple_tool($name, $desc, $schema, $handler)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_sdk_mcp_server() {
        let mut server = SdkMcpServer::new("test-server", "1.0.0");

        // Add a simple tool
        let tool = create_simple_tool(
            "greet",
            "Greet a user",
            ToolInputSchema {
                schema_type: "object".to_string(),
                properties: {
                    let mut props = HashMap::new();
                    props.insert(
                        "name".to_string(),
                        json!({"type": "string", "description": "Name to greet"}),
                    );
                    props
                },
                required: Some(vec!["name".to_string()]),
            },
            |args| async move {
                let name = args["name"].as_str().unwrap_or("stranger");
                Ok(format!("Hello, {name}!"))
            },
        );

        server.add_tool(tool);

        // Test initialize
        let init_msg = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize"
        });

        let response = server.handle_message(init_msg).await.unwrap();
        assert_eq!(response["result"]["serverInfo"]["name"], "test-server");

        // Test tools/list
        let list_msg = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        });

        let response = server.handle_message(list_msg).await.unwrap();
        assert_eq!(response["result"]["tools"][0]["name"], "greet");

        // Test tools/call
        let call_msg = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "greet",
                "arguments": {
                    "name": "Alice"
                }
            }
        });

        let response = server.handle_message(call_msg).await.unwrap();
        assert_eq!(response["result"]["content"][0]["text"], "Hello, Alice!");
    }

    // --- Helper handler for tests ---

    struct EchoHandler;

    #[async_trait]
    impl ToolHandler for EchoHandler {
        async fn execute(&self, args: Value) -> Result<ToolResult> {
            Ok(ToolResult {
                content: vec![ToolResultContent::Text {
                    text: args.to_string(),
                }],
                is_error: None,
            })
        }
    }

    fn make_echo_tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: format!("Echo tool {name}"),
            input_schema: ToolInputSchema {
                schema_type: "object".to_string(),
                properties: HashMap::new(),
                required: None,
            },
            handler: Arc::new(EchoHandler),
        }
    }

    fn make_server_with_echo() -> SdkMcpServer {
        let mut server = SdkMcpServer::new("test-server", "1.0.0");
        server.add_tool(make_echo_tool("echo"));
        server
    }

    // 1. Missing "method" field
    #[tokio::test]
    async fn test_handle_message_missing_method() {
        let server = make_server_with_echo();
        let msg = json!({"jsonrpc": "2.0", "id": 1});
        let err = server.handle_message(msg).await.unwrap_err();
        assert!(
            matches!(err, SdkError::InvalidState { .. }),
            "expected InvalidState, got: {err:?}"
        );
    }

    // 2. notifications/initialized
    #[tokio::test]
    async fn test_handle_message_notifications_initialized() {
        let server = make_server_with_echo();
        let msg = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let response = server.handle_message(msg).await.unwrap();
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["result"], json!({}));
    }

    // 3. Unknown method
    #[tokio::test]
    async fn test_handle_message_unknown_method() {
        let server = make_server_with_echo();
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "bogus/method"});
        let response = server.handle_message(msg).await.unwrap();
        assert_eq!(response["error"]["code"], -32601);
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("bogus/method")
        );
    }

    // 4. tools/call missing params
    #[tokio::test]
    async fn test_handle_message_tools_call_missing_params() {
        let server = make_server_with_echo();
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call"});
        let err = server.handle_message(msg).await.unwrap_err();
        assert!(matches!(err, SdkError::InvalidState { .. }));
    }

    // 5. tools/call missing tool name
    #[tokio::test]
    async fn test_handle_message_tools_call_missing_tool_name() {
        let server = make_server_with_echo();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"arguments": {}}
        });
        let err = server.handle_message(msg).await.unwrap_err();
        assert!(matches!(err, SdkError::InvalidState { .. }));
    }

    // 6. tools/call for non-existent tool
    #[tokio::test]
    async fn test_handle_message_tools_call_nonexistent_tool() {
        let server = make_server_with_echo();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "no_such_tool"}
        });
        let err = server.handle_message(msg).await.unwrap_err();
        assert!(matches!(err, SdkError::InvalidState { .. }));
    }

    // 7. tools/call with no arguments (uses empty default)
    #[tokio::test]
    async fn test_handle_message_tools_call_no_arguments() {
        let server = make_server_with_echo();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "echo"}
        });
        let response = server.handle_message(msg).await.unwrap();
        // EchoHandler serialises args; with no arguments the default is {}
        assert_eq!(response["result"]["content"][0]["text"], "{}");
    }

    // 8. SdkMcpServerBuilder
    #[tokio::test]
    async fn test_builder_new_version_tool_build() {
        let server = SdkMcpServerBuilder::new("builder-server")
            .version("2.0.0")
            .tool(make_echo_tool("echo"))
            .build();

        assert_eq!(server.name, "builder-server");
        assert_eq!(server.version, "2.0.0");
        assert_eq!(server.tools.len(), 1);
        assert_eq!(server.tools[0].name, "echo");

        // Verify the built server actually works
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"});
        let resp = server.handle_message(msg).await.unwrap();
        assert_eq!(resp["result"]["serverInfo"]["name"], "builder-server");
        assert_eq!(resp["result"]["serverInfo"]["version"], "2.0.0");
    }

    // 9. ToolDefinition Debug impl
    #[test]
    fn test_tool_definition_debug() {
        let tool = make_echo_tool("dbg-tool");
        let debug_str = format!("{tool:?}");
        assert!(debug_str.contains("dbg-tool"));
        assert!(debug_str.contains("<Arc<dyn ToolHandler>>"));
    }

    // 10. ToolResultContent::Image serialization
    #[test]
    fn test_tool_result_content_image_serialization() {
        let content = ToolResultContent::Image {
            data: "iVBOR...".to_string(),
            mime_type: "image/png".to_string(),
        };
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["type"], "image");
        assert_eq!(json["data"], "iVBOR...");
        assert_eq!(json["mimeType"], "image/png");
    }

    // 11. ToolResult with is_error: Some(true)
    #[test]
    fn test_tool_result_is_error_serialization() {
        let result = ToolResult {
            content: vec![ToolResultContent::Text {
                text: "something went wrong".to_string(),
            }],
            is_error: Some(true),
        };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["is_error"], true);
        assert_eq!(json["content"][0]["text"], "something went wrong");

        // Also verify None is skipped
        let result_ok = ToolResult {
            content: vec![],
            is_error: None,
        };
        let json_ok = serde_json::to_value(&result_ok).unwrap();
        assert!(json_ok.get("is_error").is_none());
    }

    // 12. SdkMcpServer::to_config
    #[test]
    fn test_to_config() {
        let server = SdkMcpServer::new("cfg-server", "1.0.0");
        let config = server.to_config();
        match &config {
            crate::types::McpServerConfig::Sdk { name, .. } => {
                assert_eq!(name, "cfg-server");
            },
            other => panic!("Expected Sdk variant, got: {other:?}"),
        }
    }

    // 13. create_simple_tool - error case in handler
    #[tokio::test]
    async fn test_create_simple_tool_error_handler() {
        let tool = create_simple_tool(
            "fail-tool",
            "A tool that always fails",
            ToolInputSchema {
                schema_type: "object".to_string(),
                properties: HashMap::new(),
                required: None,
            },
            |_args| async move {
                Err(SdkError::InvalidState {
                    message: "intentional failure".to_string(),
                })
            },
        );

        let mut server = SdkMcpServer::new("err-server", "1.0.0");
        server.add_tool(tool);

        let msg = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "fail-tool", "arguments": {}}
        });
        let err = server.handle_message(msg).await.unwrap_err();
        assert!(matches!(err, SdkError::InvalidState { .. }));
    }

    // =====================================================================
    // Protocol shape: what the CLI actually receives.
    // =====================================================================

    /// The JSON-RPC `id` is echoed verbatim, whatever its type.
    #[tokio::test]
    async fn test_request_id_is_echoed_with_its_original_type() {
        let server = make_server_with_echo();

        let numeric = server
            .handle_message(json!({"jsonrpc": "2.0", "id": 7, "method": "initialize"}))
            .await
            .unwrap();
        assert_eq!(numeric["id"], json!(7));

        let textual = server
            .handle_message(json!({"jsonrpc": "2.0", "id": "abc", "method": "tools/list"}))
            .await
            .unwrap();
        assert_eq!(textual["id"], json!("abc"));
    }

    /// A request without an `id` still produces a response carrying `"id": null`
    /// instead of omitting the field.
    #[tokio::test]
    async fn test_missing_id_is_serialised_as_null() {
        let server = make_server_with_echo();
        let response = server
            .handle_message(json!({"jsonrpc": "2.0", "method": "tools/list"}))
            .await
            .unwrap();

        assert_eq!(response["id"], Value::Null);
        assert!(response.get("id").is_some(), "the key is present and null");
    }

    /// `initialize` advertises a fixed protocol version and the tools capability.
    #[tokio::test]
    async fn test_initialize_advertises_protocol_version_and_tool_capability() {
        let server = SdkMcpServerBuilder::new("caps").version("9.9.9").build();
        let response = server
            .handle_message(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
            .await
            .unwrap();

        assert_eq!(response["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(response["result"]["capabilities"]["tools"], json!({}));
        assert_eq!(response["result"]["serverInfo"]["version"], "9.9.9");
    }

    /// A server with no tools answers `tools/list` with an empty array, never
    /// `null` — the CLI would reject the latter.
    #[tokio::test]
    async fn test_tools_list_on_an_empty_server_is_an_empty_array() {
        let server = SdkMcpServer::new("empty", "1.0.0");
        let response = server
            .handle_message(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
            .await
            .unwrap();

        assert_eq!(response["result"]["tools"], json!([]));
    }

    /// `tools/list` republishes the declared input schema, including `required`.
    #[tokio::test]
    async fn test_tools_list_publishes_the_declared_input_schema() {
        let mut server = SdkMcpServer::new("schema", "1.0.0");
        let mut properties = HashMap::new();
        properties.insert("name".to_string(), json!({"type": "string"}));
        server.add_tool(ToolDefinition {
            name: "greet".to_string(),
            description: "Greet someone".to_string(),
            input_schema: ToolInputSchema {
                schema_type: "object".to_string(),
                properties,
                required: Some(vec!["name".to_string()]),
            },
            handler: Arc::new(EchoHandler),
        });

        let response = server
            .handle_message(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
            .await
            .unwrap();
        let schema = &response["result"]["tools"][0]["inputSchema"];
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], json!(["name"]));
        assert_eq!(schema["properties"]["name"]["type"], "string");
    }

    /// The schema is advertised but **never enforced**: a tool declaring
    /// `required: ["name"]` is still invoked with `{}`.
    #[tokio::test]
    async fn test_required_arguments_are_not_validated_before_dispatch() {
        let mut server = SdkMcpServer::new("schema", "1.0.0");
        let mut properties = HashMap::new();
        properties.insert("name".to_string(), json!({"type": "string"}));
        server.add_tool(ToolDefinition {
            name: "greet".to_string(),
            description: "Greet someone".to_string(),
            input_schema: ToolInputSchema {
                schema_type: "object".to_string(),
                properties,
                required: Some(vec!["name".to_string()]),
            },
            handler: Arc::new(EchoHandler),
        });

        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": "greet", "arguments": {}}
            }))
            .await
            .unwrap();

        assert_eq!(
            response["result"]["content"][0]["text"], "{}",
            "the handler is reached with empty arguments; validation is its job"
        );
    }

    /// Non-object `arguments` are forwarded untouched instead of being refused.
    #[tokio::test]
    async fn test_non_object_arguments_are_forwarded_verbatim() {
        let server = make_server_with_echo();
        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": "echo", "arguments": "a bare string"}
            }))
            .await
            .unwrap();

        assert_eq!(
            response["result"]["content"][0]["text"],
            "\"a bare string\""
        );
    }

    /// Two tools may share a name: `add_tool` does not reject the duplicate and
    /// dispatch silently picks the first one registered.
    #[tokio::test]
    async fn test_duplicate_tool_names_are_accepted_and_the_first_one_wins() {
        struct Marker(&'static str);

        #[async_trait]
        impl ToolHandler for Marker {
            async fn execute(&self, _args: Value) -> Result<ToolResult> {
                Ok(ToolResult {
                    content: vec![ToolResultContent::Text {
                        text: self.0.to_string(),
                    }],
                    is_error: None,
                })
            }
        }

        let mut server = SdkMcpServer::new("dup", "1.0.0");
        for marker in ["first", "second"] {
            server.add_tool(ToolDefinition {
                name: "dup".to_string(),
                description: marker.to_string(),
                input_schema: ToolInputSchema {
                    schema_type: "object".to_string(),
                    properties: HashMap::new(),
                    required: None,
                },
                handler: Arc::new(Marker(marker)),
            });
        }

        assert_eq!(server.tools.len(), 2, "both registrations are kept");
        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": "dup"}
            }))
            .await
            .unwrap();
        assert_eq!(response["result"]["content"][0]["text"], "first");
    }

    /// A successful `tools/call` emits `isError: null` when the tool did not set
    /// the flag (the key is present, not omitted).
    #[tokio::test]
    async fn test_tools_call_success_reports_is_error_as_null() {
        let server = make_server_with_echo();
        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": "echo"}
            }))
            .await
            .unwrap();

        assert_eq!(response["result"]["isError"], Value::Null);
    }

    /// A tool reporting a *business* error returns `isError: true` inside a
    /// successful JSON-RPC response — it is not a protocol error.
    #[tokio::test]
    async fn test_tool_reported_error_travels_as_is_error_true() {
        struct Failing;

        #[async_trait]
        impl ToolHandler for Failing {
            async fn execute(&self, _args: Value) -> Result<ToolResult> {
                Ok(ToolResult {
                    content: vec![ToolResultContent::Text {
                        text: "disk on fire".to_string(),
                    }],
                    is_error: Some(true),
                })
            }
        }

        let mut server = SdkMcpServer::new("failing", "1.0.0");
        server.add_tool(ToolDefinition {
            name: "boom".to_string(),
            description: "always reports an error".to_string(),
            input_schema: ToolInputSchema {
                schema_type: "object".to_string(),
                properties: HashMap::new(),
                required: None,
            },
            handler: Arc::new(Failing),
        });

        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": "boom"}
            }))
            .await
            .unwrap();

        assert_eq!(response["result"]["isError"], json!(true));
        assert_eq!(response["result"]["content"][0]["text"], "disk on fire");
        assert!(response.get("error").is_none());
    }

    /// `notifications/initialized` is answered although JSON-RPC notifications
    /// take no reply: the SDK transport wraps every MCP message in a *control
    /// request* that must be answered, so the reply carries no `id`.
    #[tokio::test]
    async fn test_initialized_notification_is_answered_without_an_id() {
        let server = make_server_with_echo();
        let response = server
            .handle_message(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await
            .unwrap();

        assert_eq!(response, json!({"jsonrpc": "2.0", "result": {}}));
        assert!(response.get("id").is_none());
    }

    /// An unknown method is reported in-band (`-32601`), while a malformed
    /// envelope is reported out-of-band as a Rust error. Both are exercised
    /// here to pin the asymmetry.
    #[tokio::test]
    async fn test_unknown_method_is_in_band_but_a_missing_method_is_not() {
        let server = make_server_with_echo();

        let unknown = server
            .handle_message(json!({"jsonrpc": "2.0", "id": 4, "method": "resources/list"}))
            .await
            .unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
        assert_eq!(unknown["id"], json!(4));

        let malformed = server
            .handle_message(json!({"jsonrpc": "2.0", "id": 4, "method": 42}))
            .await
            .unwrap_err();
        assert!(
            matches!(&malformed, SdkError::InvalidState { message } if message.contains("Missing method")),
            "got {malformed:?}"
        );
    }

    /// The exact path `internal_query` uses: the config stores the server as
    /// `Arc<dyn Any>` and downcasts it back before dispatching.
    #[tokio::test]
    async fn test_config_instance_downcasts_back_to_a_working_server() {
        let mut server = SdkMcpServer::new("downcast", "1.2.3");
        server.add_tool(make_echo_tool("echo"));

        let config = server.to_config();
        let crate::types::McpServerConfig::Sdk { name, instance } = &config else {
            panic!("expected the Sdk variant, got {config:?}");
        };
        assert_eq!(name, "downcast");

        let recovered = instance
            .downcast_ref::<SdkMcpServer>()
            .expect("internal_query downcasts to SdkMcpServer");
        let response = recovered
            .handle_message(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
            .await
            .unwrap();
        assert_eq!(response["result"]["serverInfo"]["version"], "1.2.3");
    }

    /// The `tool!` macro is just sugar over `create_simple_tool`.
    #[tokio::test]
    async fn test_tool_macro_builds_a_working_definition() {
        let tool = crate::tool!(
            "shout",
            "Uppercase its input",
            ToolInputSchema {
                schema_type: "object".to_string(),
                properties: HashMap::new(),
                required: None,
            },
            |args: Value| async move { Ok(args["text"].as_str().unwrap_or_default().to_uppercase()) }
        );
        assert_eq!(tool.name, "shout");

        let mut server = SdkMcpServer::new("macro", "1.0.0");
        server.add_tool(tool);
        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": "shout", "arguments": {"text": "hey"}}
            }))
            .await
            .unwrap();
        assert_eq!(response["result"]["content"][0]["text"], "HEY");
    }

    /// `ToolResult` serialises its flag as `is_error`, while the wire format
    /// built by `handle_message` uses the MCP spelling `isError`. Both are
    /// pinned here so a future refactor cannot silently swap one for the other.
    #[test]
    fn test_tool_result_field_name_differs_from_the_wire_field_name() {
        let result = ToolResult {
            content: vec![],
            is_error: Some(false),
        };
        let serialised = serde_json::to_value(&result).unwrap();
        assert!(serialised.get("is_error").is_some());
        assert!(
            serialised.get("isError").is_none(),
            "the struct itself is not MCP-shaped; handle_message does the mapping"
        );
    }

    /// `ToolInputSchema` round-trips, and `required: None` disappears from the
    /// JSON instead of becoming `null`.
    #[test]
    fn test_tool_input_schema_roundtrip_omits_absent_required() {
        let schema = ToolInputSchema {
            schema_type: "object".to_string(),
            properties: HashMap::new(),
            required: None,
        };
        let serialised = serde_json::to_value(&schema).unwrap();
        assert_eq!(serialised, json!({"type": "object", "properties": {}}));

        let back: ToolInputSchema = serde_json::from_value(serialised).unwrap();
        assert_eq!(back.schema_type, "object");
        assert!(back.required.is_none());
    }
}

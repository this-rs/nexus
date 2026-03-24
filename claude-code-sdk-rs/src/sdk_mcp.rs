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
}

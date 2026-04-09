use crate::models::openai::{FunctionCall, Tool};
use serde_json::Value;
use tracing::info;

/// Detects if Claude's response contains JSON that should be formatted as a tool call
pub fn detect_and_convert_tool_call(
    content: &str,
    requested_tools: &Option<Vec<Tool>>,
) -> Option<FunctionCall> {
    // Only process if tools were requested
    if requested_tools.is_none() {
        return None;
    }

    // Try to extract JSON from the content
    let json_result = extract_json_from_content(content);

    if let Some(json_value) = json_result {
        info!("Detected JSON in Claude's response: {:?}", json_value);

        // Check if this looks like a tool call
        if let Some(tool_name) = detect_tool_name(&json_value, requested_tools) {
            info!("Converting to tool call: {}", tool_name);

            // Convert the JSON to a function call
            return Some(FunctionCall {
                name: tool_name,
                arguments: json_value.to_string(),
            });
        } else if let Some(tools) = requested_tools {
            // If no specific tool match but JSON is valid and tools were requested,
            // use the first tool name
            if !tools.is_empty() && json_value.is_object() {
                info!("Using first tool as default: {}", tools[0].function.name);
                return Some(FunctionCall {
                    name: tools[0].function.name.clone(),
                    arguments: json_value.to_string(),
                });
            }
        }
    }

    None
}

/// Extracts JSON from Claude's response content
fn extract_json_from_content(content: &str) -> Option<Value> {
    // Try to parse the entire content as JSON first
    if let Ok(json) = serde_json::from_str::<Value>(content.trim()) {
        return Some(json);
    }

    // Look for JSON blocks in the content
    // Claude often wraps JSON in markdown code blocks
    if let Some(start) = content.find("```json") {
        let after_marker = &content[start + 7..];
        if let Some(end) = after_marker.find("```") {
            let json_str = after_marker[..end].trim();
            if let Ok(json) = serde_json::from_str::<Value>(json_str) {
                return Some(json);
            }
        }
    }

    // Look for any JSON object in the content
    if let Some(start) = content.find('{')
        && let Some(end) = find_matching_brace(&content[start..])
    {
        let json_str = &content[start..start + end + 1];
        if let Ok(json) = serde_json::from_str::<Value>(json_str) {
            return Some(json);
        }
    }

    None
}

/// Finds the matching closing brace for a JSON object
fn find_matching_brace(s: &str) -> Option<usize> {
    let mut depth = 0;
    let mut in_string = false;
    let mut escape_next = false;

    for (i, ch) in s.chars().enumerate() {
        if escape_next {
            escape_next = false;
            continue;
        }

        match ch {
            '\\' if in_string => escape_next = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            },
            _ => {},
        }
    }

    None
}

/// Detects if the JSON matches any of the requested tools
fn detect_tool_name(json: &Value, requested_tools: &Option<Vec<Tool>>) -> Option<String> {
    // Check if the JSON has a function/action/tool field
    if let Some(tool_name) = json
        .get("function")
        .or_else(|| json.get("action"))
        .or_else(|| json.get("tool"))
        .or_else(|| json.get("name"))
        .and_then(|v| v.as_str())
    {
        return Some(tool_name.to_string());
    }

    // Check if the JSON structure matches any of the requested tools
    if let Some(tools) = requested_tools {
        for tool in tools {
            if tool.tool_type == "function"
                && json_matches_tool_schema(json, &tool.function.parameters)
            {
                info!("JSON matches schema for tool: {}", tool.function.name);
                return Some(tool.function.name.clone());
            }
        }
    }

    None
}

/// Checks if JSON matches a tool's parameter schema
fn json_matches_tool_schema(json: &Value, schema: &Value) -> bool {
    // Check if the JSON object matches the schema structure
    if let (Some(json_obj), Some(schema_obj)) = (json.as_object(), schema.as_object())
        && let Some(properties) = schema_obj.get("properties").and_then(|p| p.as_object())
    {
        // Check if JSON has all required properties
        if let Some(required) = schema_obj.get("required").and_then(|r| r.as_array()) {
            let required_props: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();

            // All required properties must be present
            for req_prop in &required_props {
                if !json_obj.contains_key(*req_prop) {
                    return false;
                }
            }

            // If all required properties are present, it's a match
            return true;
        } else {
            // No required properties specified, check if JSON has any of the schema properties
            let mut matches = 0;
            let total_props = properties.len();

            for (key, _) in properties {
                if json_obj.contains_key(key) {
                    matches += 1;
                }
            }

            // Consider it a match if at least 50% of properties match
            return matches > 0 && (matches * 2 >= total_props);
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::openai::FunctionDefinition;
    use serde_json::json;

    // ── Helper to build a Tool ──
    fn make_tool(name: &str, params: Value) -> Tool {
        Tool {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: name.to_string(),
                description: Some(format!("{name} tool")),
                parameters: params,
            },
        }
    }

    // ═══════════════════════════════════════════════════════════════
    //  extract_json_from_content
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn test_extract_json_direct() {
        let content = r#"{"url": "https://example.com", "action": "preview"}"#;
        let result = extract_json_from_content(content);
        assert!(result.is_some());
        assert_eq!(result.unwrap()["url"], "https://example.com");
    }

    #[test]
    fn test_extract_json_direct_with_whitespace() {
        let content = "   {\"key\": \"value\"}   ";
        let result = extract_json_from_content(content);
        assert!(result.is_some());
        assert_eq!(result.unwrap()["key"], "value");
    }

    #[test]
    fn test_extract_json_from_markdown_block() {
        let content = "Here's the result:\n```json\n{\"url\": \"https://example.com\"}\n```";
        let result = extract_json_from_content(content);
        assert!(result.is_some());
        assert_eq!(result.unwrap()["url"], "https://example.com");
    }

    #[test]
    fn test_extract_json_from_markdown_block_multiline() {
        let content = "Result:\n```json\n{\n  \"a\": 1,\n  \"b\": 2\n}\n```\nDone.";
        let result = extract_json_from_content(content);
        assert!(result.is_some());
        let v = result.unwrap();
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"], 2);
    }

    #[test]
    fn test_extract_json_embedded_in_text() {
        let content = r#"The function call is {"url": "https://example.com"} for preview"#;
        let result = extract_json_from_content(content);
        assert!(result.is_some());
    }

    #[test]
    fn test_extract_json_nested_braces() {
        let content = r#"Here: {"outer": {"inner": 42}}"#;
        let result = extract_json_from_content(content);
        assert!(result.is_some());
        assert_eq!(result.unwrap()["outer"]["inner"], 42);
    }

    #[test]
    fn test_extract_json_with_escaped_quotes() {
        let content = r#"{"msg": "he said \"hello\""}"#;
        let result = extract_json_from_content(content);
        assert!(result.is_some());
    }

    #[test]
    fn test_extract_json_no_json_returns_none() {
        let content = "This is just plain text with no JSON.";
        let result = extract_json_from_content(content);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_json_invalid_json_returns_none() {
        let content = "{not valid json at all}";
        let result = extract_json_from_content(content);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_json_empty_string() {
        assert!(extract_json_from_content("").is_none());
    }

    #[test]
    fn test_extract_json_array_direct() {
        let content = r#"[1, 2, 3]"#;
        let result = extract_json_from_content(content);
        // Direct parse succeeds for arrays
        assert!(result.is_some());
    }

    #[test]
    fn test_extract_json_markdown_block_without_closing() {
        // Unclosed markdown block — json block extraction fails, falls through to brace search
        let content = "```json\n{\"a\": 1}\nno closing ticks";
        let result = extract_json_from_content(content);
        // The brace-matching fallback should still find the JSON
        assert!(result.is_some());
    }

    // ═══════════════════════════════════════════════════════════════
    //  find_matching_brace
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn test_find_matching_brace_simple() {
        assert_eq!(find_matching_brace("{}"), Some(1));
    }

    #[test]
    fn test_find_matching_brace_nested() {
        assert_eq!(find_matching_brace("{\"a\":{\"b\":1}}"), Some(12));
    }

    #[test]
    fn test_find_matching_brace_with_string_braces() {
        // Braces inside strings should be ignored
        let s = r#"{"key": "val{ue}"}"#;
        let result = find_matching_brace(s);
        assert!(result.is_some());
        // The entire object should be matched
        assert_eq!(&s[..=result.unwrap()], s);
    }

    #[test]
    fn test_find_matching_brace_unclosed() {
        assert_eq!(find_matching_brace("{\"a\": 1"), None);
    }

    #[test]
    fn test_find_matching_brace_empty_object() {
        assert_eq!(find_matching_brace("{}"), Some(1));
    }

    #[test]
    fn test_find_matching_brace_escaped_quote() {
        // Escaped quote inside string should not toggle in_string
        let s = r#"{"k": "val\"ue"}"#;
        let result = find_matching_brace(s);
        assert!(result.is_some());
    }

    // ═══════════════════════════════════════════════════════════════
    //  detect_tool_name
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn test_detect_tool_name_action_field() {
        let json = json!({"url": "https://example.com", "action": "preview"});
        let result = detect_tool_name(&json, &None);
        assert_eq!(result, Some("preview".to_string()));
    }

    #[test]
    fn test_detect_tool_name_function_field() {
        let json = json!({"function": "my_func", "arg": 1});
        assert_eq!(detect_tool_name(&json, &None), Some("my_func".to_string()));
    }

    #[test]
    fn test_detect_tool_name_tool_field() {
        let json = json!({"tool": "search", "query": "test"});
        assert_eq!(detect_tool_name(&json, &None), Some("search".to_string()));
    }

    #[test]
    fn test_detect_tool_name_name_field() {
        let json = json!({"name": "get_weather", "location": "Paris"});
        assert_eq!(
            detect_tool_name(&json, &None),
            Some("get_weather".to_string())
        );
    }

    #[test]
    fn test_detect_tool_name_priority_function_over_action() {
        // "function" is checked first
        let json = json!({"function": "fn1", "action": "act1"});
        assert_eq!(detect_tool_name(&json, &None), Some("fn1".to_string()));
    }

    #[test]
    fn test_detect_tool_name_schema_match_with_required() {
        let json = json!({"url": "https://example.com"});
        let tool = make_tool(
            "url_preview",
            json!({
                "type": "object",
                "properties": {"url": {"type": "string"}},
                "required": ["url"]
            }),
        );
        let result = detect_tool_name(&json, &Some(vec![tool]));
        assert_eq!(result, Some("url_preview".to_string()));
    }

    #[test]
    fn test_detect_tool_name_schema_match_missing_required() {
        let json = json!({"other": "value"});
        let tool = make_tool(
            "url_preview",
            json!({
                "type": "object",
                "properties": {"url": {"type": "string"}},
                "required": ["url"]
            }),
        );
        let result = detect_tool_name(&json, &Some(vec![tool]));
        assert_eq!(result, None);
    }

    #[test]
    fn test_detect_tool_name_no_match_returns_none() {
        let json = json!({"random": "data"});
        let tool = make_tool(
            "specific_tool",
            json!({
                "type": "object",
                "properties": {"needed": {"type": "string"}},
                "required": ["needed"]
            }),
        );
        let result = detect_tool_name(&json, &Some(vec![tool]));
        assert_eq!(result, None);
    }

    // ═══════════════════════════════════════════════════════════════
    //  json_matches_tool_schema
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn test_schema_match_all_required_present() {
        let json = json!({"a": 1, "b": 2});
        let schema = json!({
            "type": "object",
            "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
            "required": ["a", "b"]
        });
        assert!(json_matches_tool_schema(&json, &schema));
    }

    #[test]
    fn test_schema_match_missing_one_required() {
        let json = json!({"a": 1});
        let schema = json!({
            "type": "object",
            "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
            "required": ["a", "b"]
        });
        assert!(!json_matches_tool_schema(&json, &schema));
    }

    #[test]
    fn test_schema_match_no_required_field_50_percent_rule() {
        // No "required" array — uses the >=50% property match heuristic
        let json = json!({"a": 1, "b": 2});
        let schema = json!({
            "type": "object",
            "properties": {"a": {}, "b": {}, "c": {}}
        });
        // 2 out of 3 matched (67%) → should match
        assert!(json_matches_tool_schema(&json, &schema));
    }

    #[test]
    fn test_schema_match_no_required_below_50_percent() {
        let json = json!({"a": 1});
        let schema = json!({
            "type": "object",
            "properties": {"a": {}, "b": {}, "c": {}}
        });
        // 1 out of 3 (33%) → should NOT match
        assert!(!json_matches_tool_schema(&json, &schema));
    }

    #[test]
    fn test_schema_match_no_required_zero_properties_matched() {
        let json = json!({"x": 1});
        let schema = json!({
            "type": "object",
            "properties": {"a": {}, "b": {}}
        });
        assert!(!json_matches_tool_schema(&json, &schema));
    }

    #[test]
    fn test_schema_match_non_object_json() {
        let json = json!("just a string");
        let schema = json!({
            "type": "object",
            "properties": {"a": {}}
        });
        assert!(!json_matches_tool_schema(&json, &schema));
    }

    #[test]
    fn test_schema_match_non_object_schema() {
        let json = json!({"a": 1});
        let schema = json!("not an object schema");
        assert!(!json_matches_tool_schema(&json, &schema));
    }

    // ═══════════════════════════════════════════════════════════════
    //  detect_and_convert_tool_call (integration of the above)
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn test_detect_and_convert_no_tools_requested() {
        let result = detect_and_convert_tool_call(r#"{"action": "test"}"#, &None);
        assert!(result.is_none());
    }

    #[test]
    fn test_detect_and_convert_with_action_field() {
        let tools = vec![make_tool(
            "preview",
            json!({"type": "object", "properties": {}}),
        )];
        let content = r#"{"action": "preview", "url": "https://example.com"}"#;
        let result = detect_and_convert_tool_call(content, &Some(tools));
        assert!(result.is_some());
        let fc = result.unwrap();
        assert_eq!(fc.name, "preview");
    }

    #[test]
    fn test_detect_and_convert_falls_back_to_first_tool() {
        // JSON is a valid object but has no function/action/tool/name fields
        // and doesn't match schema required fields — falls back to first tool
        let tools = vec![make_tool(
            "default_tool",
            json!({
                "type": "object",
                "properties": {"x": {"type": "number"}},
                "required": ["x"]
            }),
        )];
        // JSON has no "x" required field → schema match fails
        // But JSON is a valid object, so falls back to first tool
        let content = r#"{"something_else": 42}"#;
        let result = detect_and_convert_tool_call(content, &Some(tools));
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "default_tool");
    }

    #[test]
    fn test_detect_and_convert_plain_text_returns_none() {
        let tools = vec![make_tool(
            "my_tool",
            json!({"type": "object", "properties": {}}),
        )];
        let result = detect_and_convert_tool_call("Just some plain text", &Some(tools));
        assert!(result.is_none());
    }

    #[test]
    fn test_detect_and_convert_json_in_markdown() {
        let tools = vec![make_tool(
            "search",
            json!({"type": "object", "properties": {"query": {}}, "required": ["query"]}),
        )];
        let content = "Result:\n```json\n{\"query\": \"test\"}\n```";
        let result = detect_and_convert_tool_call(content, &Some(tools));
        assert!(result.is_some());
        let fc = result.unwrap();
        assert_eq!(fc.name, "search");
        // arguments should be the JSON string
        let args: Value = serde_json::from_str(&fc.arguments).unwrap();
        assert_eq!(args["query"], "test");
    }

    #[test]
    fn test_detect_and_convert_empty_tools_vec() {
        // Tools is Some but empty vec — should not crash, returns None for fallback
        let content = r#"{"data": 1}"#;
        let result = detect_and_convert_tool_call(content, &Some(vec![]));
        assert!(result.is_none());
    }
}

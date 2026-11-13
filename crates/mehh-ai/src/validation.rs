use crate::types::{ToolCall, ToolDefinition};
use serde_json::{Map, Value};

/// Validates and coerces tool call arguments against a tool's JSON Schema definition.
pub fn validate_tool_arguments(
    tool: &ToolDefinition,
    tool_call: &ToolCall,
) -> Result<Map<String, Value>, String> {
    if let Some(ref err) = tool_call.arguments_error {
        return Err(err.clone());
    }

    let mut args = tool_call.arguments.clone();

    let properties = tool
        .parameters
        .get("properties")
        .and_then(|p| p.as_object());

    // 1. Coerce values where schema defines expected types
    if let Some(props) = properties {
        for (prop_name, prop_schema) in props {
            if let Some(val) = args.get_mut(prop_name) {
                coerce_value(prop_schema, val);
            }
        }
    }

    // 2. Check required properties
    if let Some(required) = tool.parameters.get("required").and_then(|r| r.as_array()) {
        for req in required {
            if let Some(req_name) = req.as_str() {
                match args.get(req_name) {
                    None | Some(Value::Null) => {
                        return Err(format!(
                            "Missing required property \"{}\" for tool \"{}\"",
                            req_name, tool.name
                        ));
                    }
                    _ => {}
                }
            }
        }
    }

    // 3. Validate types of all present non-null properties
    if let Some(props) = properties {
        for (prop_name, val) in &args {
            if val.is_null() {
                continue;
            }
            if let Some(prop_schema) = props.get(prop_name) {
                validate_type(prop_name, prop_schema, val, &tool.name)?;
            }
        }
    }

    Ok(args)
}

fn coerce_value(schema: &Value, val: &mut Value) {
    let type_val = schema.get("type");
    let matches_type = |expected: &str| -> bool {
        match type_val {
            Some(Value::String(s)) => s == expected,
            Some(Value::Array(arr)) => arr.iter().any(|v| v.as_str() == Some(expected)),
            _ => false,
        }
    };

    if matches_type("integer") {
        if let Value::String(s) = val {
            if let Ok(i) = s.trim().parse::<i64>() {
                *val = Value::Number(i.into());
                return;
            }
        }
    }

    if matches_type("number") {
        if let Value::String(s) = val {
            if let Ok(f) = s.trim().parse::<f64>() {
                if let Some(num) = serde_json::Number::from_f64(f) {
                    *val = Value::Number(num);
                    return;
                }
            }
        }
    }

    if matches_type("boolean") {
        if let Value::String(s) = val {
            let lower = s.trim().to_lowercase();
            if lower == "true" {
                *val = Value::Bool(true);
                return;
            } else if lower == "false" {
                *val = Value::Bool(false);
                return;
            }
        }
    }

    if matches_type("array") {
        if let Value::String(s) = val {
            if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(s) {
                *val = Value::Array(arr);
                return;
            }
        }
    }

    if matches_type("object") {
        if let Value::String(s) = val {
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(s) {
                *val = Value::Object(map);
            }
        }
    }
}

fn validate_type(
    prop_name: &str,
    schema: &Value,
    val: &Value,
    tool_name: &str,
) -> Result<(), String> {
    let type_val = match schema.get("type") {
        Some(t) => t,
        None => return Ok(()),
    };

    let check_single = |expected: &str| -> bool {
        match expected {
            "string" => val.is_string(),
            "number" => val.is_number(),
            "integer" => {
                if val.as_i64().is_some() || val.as_u64().is_some() {
                    true
                } else if let Some(f) = val.as_f64() {
                    f.fract() == 0.0
                } else {
                    false
                }
            }
            "boolean" => val.is_boolean(),
            "array" => val.is_array(),
            "object" => val.is_object(),
            "null" => val.is_null(),
            _ => true,
        }
    };

    let ok = match type_val {
        Value::String(s) => check_single(s.as_str()),
        Value::Array(arr) => arr
            .iter()
            .any(|item| item.as_str().is_some_and(check_single)),
        _ => true,
    };

    if !ok {
        return Err(format!(
            "Property \"{}\" in tool \"{}\" expected type {:?}, got {:?}",
            prop_name, tool_name, type_val, val
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_tool() -> ToolDefinition {
        ToolDefinition {
            name: "test_tool".to_string(),
            description: "A tool for testing".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "count": { "type": "integer" },
                    "ratio": { "type": "number" },
                    "enabled": { "type": "boolean" },
                    "tags": { "type": "array" },
                    "meta": { "type": "object" },
                    "name": { "type": "string" }
                },
                "required": ["name", "count"]
            }),
        }
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn test_valid_args_and_coercion() {
        let tool = create_test_tool();
        let mut raw_args = Map::new();
        raw_args.insert("name".to_string(), Value::String("Alice".to_string()));
        raw_args.insert("count".to_string(), Value::String("42".to_string()));
        raw_args.insert("ratio".to_string(), Value::String("3.14".to_string()));
        raw_args.insert("enabled".to_string(), Value::String("true".to_string()));
        raw_args.insert("tags".to_string(), Value::String("[\"a\", \"b\"]".to_string()));
        raw_args.insert("meta".to_string(), Value::String("{\"k\": \"v\"}".to_string()));

        let call = ToolCall {
            id: "call_1".to_string(),
            name: "test_tool".to_string(),
            arguments: raw_args,
            arguments_error: None,
            thought_signature: None,
        };

        let result = validate_tool_arguments(&tool, &call).unwrap();
        assert_eq!(result.get("count"), Some(&Value::Number(42.into())));
        assert_eq!(result.get("ratio"), Some(&serde_json::json!(3.14)));
        assert_eq!(result.get("enabled"), Some(&Value::Bool(true)));
        assert_eq!(
            result.get("tags"),
            Some(&serde_json::json!(["a", "b"]))
        );
        assert_eq!(
            result.get("meta"),
            Some(&serde_json::json!({ "k": "v" }))
        );
    }

    #[test]
    fn test_missing_required() {
        let tool = create_test_tool();
        let mut raw_args = Map::new();
        raw_args.insert("name".to_string(), Value::String("Alice".to_string()));
        // "count" is missing

        let call = ToolCall {
            id: "call_2".to_string(),
            name: "test_tool".to_string(),
            arguments: raw_args,
            arguments_error: None,
            thought_signature: None,
        };

        let err = validate_tool_arguments(&tool, &call).unwrap_err();
        assert!(err.contains("Missing required property \"count\""));
    }

    #[test]
    fn test_arguments_error_forwarded() {
        let tool = create_test_tool();
        let call = ToolCall {
            id: "call_3".to_string(),
            name: "test_tool".to_string(),
            arguments: Map::new(),
            arguments_error: Some("Syntax error in JSON arguments".to_string()),
            thought_signature: None,
        };

        let err = validate_tool_arguments(&tool, &call).unwrap_err();
        assert_eq!(err, "Syntax error in JSON arguments");
    }

    #[test]
    fn test_type_mismatch() {
        let tool = create_test_tool();
        let mut raw_args = Map::new();
        raw_args.insert("name".to_string(), Value::Number(123.into())); // expected string
        raw_args.insert("count".to_string(), Value::Number(42.into()));

        let call = ToolCall {
            id: "call_4".to_string(),
            name: "test_tool".to_string(),
            arguments: raw_args,
            arguments_error: None,
            thought_signature: None,
        };

        let err = validate_tool_arguments(&tool, &call).unwrap_err();
        assert!(err.contains("expected type"));
    }
}

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

    // 3. Validate types, constraints and nested schemas of all present properties.
    if let Some(props) = properties {
        for (prop_name, val) in &args {
            if let Some(prop_schema) = props.get(prop_name) {
                validate_schema(prop_name, prop_schema, val, &tool.name)?;
            }
        }
    }
    check_additional_properties(&tool.parameters, properties, &args, &tool.name)?;

    Ok(args)
}

/// Rejects undeclared properties when a schema sets `additionalProperties: false`,
/// and validates them against a schema object when one is supplied.
fn check_additional_properties(
    schema: &Value,
    properties: Option<&Map<String, Value>>,
    map: &Map<String, Value>,
    tool_name: &str,
) -> Result<(), String> {
    let additional = match schema.get("additionalProperties") {
        Some(Value::Bool(false)) => {
            if let Some(name) = map.keys().find(|name| {
                !properties
                    .map(|p| p.contains_key(*name))
                    .unwrap_or(false)
            }) {
                return Err(format!(
                    "Property \"{name}\" in tool \"{tool_name}\" is not allowed (additionalProperties: false)"
                ));
            }
            return Ok(());
        }
        Some(schema @ Value::Object(_)) => schema,
        _ => return Ok(()),
    };
    for (name, val) in map {
        let declared = properties
            .map(|p| p.contains_key(name))
            .unwrap_or(false);
        if !declared {
            validate_schema(name, additional, val, tool_name)?;
        }
    }
    Ok(())
}

/// Validates a value against every supported JSON Schema keyword used by tools.
fn validate_schema(
    path: &str,
    schema: &Value,
    val: &Value,
    tool_name: &str,
) -> Result<(), String> {
    // A present `null` on an optional property means "not provided" for tools.
    // Required properties already reject null in the caller's required check.
    if val.is_null() {
        return Ok(());
    }
    validate_type(path, schema, val, tool_name)?;

    if let Some(allowed) = schema.get("enum").and_then(|e| e.as_array()) {
        if !allowed.iter().any(|v| v == val) {
            return Err(format!(
                "Property \"{path}\" in tool \"{tool_name}\" has value {val} not in enum {allowed:?}"
            ));
        }
    }

    if let Some(number) = val.as_f64() {
        if let Some(min) = schema.get("minimum").and_then(|m| m.as_f64()) {
            if number < min {
                return Err(format!(
                    "Property \"{path}\" in tool \"{tool_name}\" is below minimum {min}: {number}"
                ));
            }
        }
        if let Some(max) = schema.get("maximum").and_then(|m| m.as_f64()) {
            if number > max {
                return Err(format!(
                    "Property \"{path}\" in tool \"{tool_name}\" is above maximum {max}: {number}"
                ));
            }
        }
    }

    if let Some(obj) = val.as_object() {
        let nested_props = schema.get("properties").and_then(|p| p.as_object());
        if let Some(nested_props) = nested_props {
            for (name, child) in obj {
                if let Some(child_schema) = nested_props.get(name) {
                    let child_path = format!("{path}.{name}");
                    validate_schema(&child_path, child_schema, child, tool_name)?;
                }
            }
        }
        // `required` applies to an object schema even when it declares no
        // `properties` (e.g. `{"type":"object","required":["x"]}`).
        if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
            for req in required {
                if let Some(req_name) = req.as_str() {
                    if !obj.get(req_name).is_some_and(|v| !v.is_null()) {
                        return Err(format!(
                            "Missing required property \"{req_name}\" in \"{path}\" for tool \"{tool_name}\""
                        ));
                    }
                }
            }
        }
        check_additional_properties(schema, nested_props, obj, tool_name)?;
    }

    if let Some(arr) = val.as_array() {
        if let Some(max_items) = schema.get("maxItems").and_then(|m| m.as_u64()) {
            if arr.len() as u64 > max_items {
                return Err(format!(
                    "Property \"{path}\" in tool \"{tool_name}\" has {} items, exceeding maxItems {max_items}",
                    arr.len()
                ));
            }
        }
        if let Some(items) = schema.get("items") {
            for (index, item) in arr.iter().enumerate() {
                let item_path = format!("{path}[{index}]");
                validate_schema(&item_path, items, item, tool_name)?;
            }
        }
    }

    Ok(())
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

    // Coerce nested values too: models routinely stringify nested numbers and
    // arrays, and the type check runs after this pass.
    match val {
        Value::Object(map) => {
            if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
                for (name, child) in map {
                    if let Some(child_schema) = props.get(name) {
                        coerce_value(child_schema, child);
                    }
                }
            }
        }
        Value::Array(arr) => {
            if let Some(items) = schema.get("items") {
                for item in arr {
                    coerce_value(items, item);
                }
            }
        }
        _ => {}
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

    fn call_with(
        name: &str,
        parameters: serde_json::Value,
        args: serde_json::Value,
    ) -> Result<Map<String, Value>, String> {
        let tool = ToolDefinition {
            name: name.to_string(),
            description: "test tool".to_string(),
            parameters,
        };
        let call = ToolCall {
            id: "call_constraints".to_string(),
            name: name.to_string(),
            arguments: args.as_object().cloned().unwrap_or_default(),
            arguments_error: None,
            thought_signature: None,
        };
        validate_tool_arguments(&tool, &call)
    }

    #[test]
    fn validation_rejects_minimum_and_unknown_properties() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "n": { "type": "integer", "minimum": 1 } },
            "additionalProperties": false
        });
        let err = call_with("t", schema, serde_json::json!({ "n": 0, "extra": "x" })).unwrap_err();
        assert!(
            err.contains("minimum") || err.contains("additional"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validation_rejects_enum_mismatch() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "mode": { "type": "string", "enum": ["a", "b"] } }
        });
        let err = call_with("t", schema, serde_json::json!({ "mode": "c" })).unwrap_err();
        assert!(err.contains("enum"), "unexpected error: {err}");
    }

    #[test]
    fn validation_rejects_nested_array_item_type_mismatch() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "items": { "type": "array", "items": { "type": "integer" } }
            }
        });
        let err = call_with("t", schema, serde_json::json!({ "items": [1, "bad"] })).unwrap_err();
        assert!(err.contains("expected type"), "unexpected error: {err}");
    }

    #[test]
    fn validation_treats_null_as_absent_for_optional_fields() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "id": { "type": "integer" }
            },
            "required": ["id"]
        });
        let ok = call_with("t", schema.clone(), serde_json::json!({ "name": null, "id": 1 }))
            .unwrap();
        assert!(ok.contains_key("name"), "optional null must stay present");

        let err = call_with("t", schema, serde_json::json!({ "name": null, "id": null }))
            .unwrap_err();
        assert!(
            err.contains("Missing required property"),
            "required null must still be rejected: {err}"
        );
    }

    #[test]
    fn validation_coerces_nested_object_and_array_values() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "config": {
                    "type": "object",
                    "properties": { "retries": { "type": "integer" } }
                },
                "ids": { "type": "array", "items": { "type": "integer" } }
            }
        });
        let args = call_with(
            "t",
            schema,
            serde_json::json!({ "config": { "retries": "3" }, "ids": ["1", "2"] }),
        )
        .unwrap();
        assert_eq!(args["config"]["retries"], serde_json::json!(3));
        assert_eq!(args["ids"], serde_json::json!([1, 2]));
    }

    #[test]
    fn validation_coerces_stringified_array_and_its_items() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "ids": { "type": "array", "items": { "type": "integer" } }
            }
        });
        let args = call_with(
            "t",
            schema,
            serde_json::json!({ "ids": "[\"1\", \"2\"]" }),
        )
        .unwrap();
        assert_eq!(args["ids"], serde_json::json!([1, 2]));
    }

    #[test]
    fn validation_rejects_array_exceeding_max_items() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "tasks": { "type": "array", "items": { "type": "string" }, "maxItems": 2 }
            }
        });
        let err = call_with(
            "subagent",
            schema,
            serde_json::json!({ "tasks": ["a", "b", "c"] }),
        )
        .unwrap_err();
        assert!(err.contains("maxItems"), "unexpected error: {err}");
    }

    #[test]
    fn validation_rejects_nested_required_without_properties() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "config": { "type": "object", "required": ["token"] }
            }
        });
        let err = call_with("t", schema, serde_json::json!({ "config": {} })).unwrap_err();
        assert!(
            err.contains("Missing required property \"token\""),
            "unexpected error: {err}"
        );
    }
}

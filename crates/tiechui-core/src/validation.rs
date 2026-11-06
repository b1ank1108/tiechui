use jsonschema::error::{TypeKind, ValidationErrorKind};
use serde_json::{Number, Value};
use tiechui_model::ToolCall;
use tiechui_tools::ToolRegistry;

pub(crate) fn validate(registry: &ToolRegistry, call: &ToolCall) -> Result<Value, String> {
    let tool = registry
        .get(&call.name)
        .ok_or_else(|| format!("Tool {} not found", call.name))?;
    // Remote schema retrieval is disabled at the dependency's feature boundary.
    let validator =
        jsonschema::validator_for(&tool.spec().input_schema).map_err(|error| error.to_string())?;
    let mut arguments = call.arguments.clone();
    normalize_optional_nulls(&mut arguments, &tool.spec().input_schema);
    coerce(&mut arguments, &tool.spec().input_schema);
    let errors: Vec<_> = validator
        .iter_errors(&arguments)
        .map(|error| {
            let mut path = error
                .instance_path
                .to_string()
                .trim_start_matches('/')
                .replace('/', ".");
            let message = match &error.kind {
                ValidationErrorKind::Required { property } => {
                    let property = property.as_str().unwrap_or_default();
                    if !path.is_empty() {
                        path.push('.');
                    }
                    path.push_str(property);
                    format!("must have required properties {property}")
                }
                ValidationErrorKind::Type {
                    kind: TypeKind::Single(kind),
                } => format!("must be {kind}"),
                _ => error.to_string(),
            };
            if path.is_empty() {
                path.push_str("root");
            }
            format!("  - {path}: {message}")
        })
        .collect();
    if errors.is_empty() {
        return Ok(arguments);
    }
    Err(format!(
        "Validation failed for tool \"{}\":\n{}\n\nReceived arguments:\n{}",
        call.name,
        errors.join("\n"),
        serde_json::to_string_pretty(&call.arguments).map_err(|error| error.to_string())?
    ))
}

fn accepts(schema: &Value, value: &Value) -> Option<bool> {
    jsonschema::validator_for(schema)
        .ok()
        .map(|validator| validator.is_valid(value))
}

fn normalize_optional_nulls(value: &mut Value, schema: &Value) {
    match value {
        Value::Object(object) => {
            if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
                for (name, property) in properties {
                    let required = schema
                        .get("required")
                        .and_then(Value::as_array)
                        .is_some_and(|required| {
                            required.iter().any(|key| key.as_str() == Some(name))
                        });
                    if object.get(name).is_some_and(Value::is_null)
                        && !required
                        && !property.get("$ref").is_some_and(Value::is_string)
                        && accepts(property, &Value::Null) == Some(false)
                    {
                        object.remove(name);
                    } else if let Some(value) = object.get_mut(name) {
                        normalize_optional_nulls(value, property);
                    }
                }
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                if let Some(schema) = item_schema(schema, index) {
                    normalize_optional_nulls(item, schema);
                }
            }
        }
        _ => {}
    }
}

fn coerce(value: &mut Value, schema: &Value) {
    if let Some(schemas) = schema.get("allOf").and_then(Value::as_array) {
        for schema in schemas {
            coerce(value, schema);
        }
    }
    for keyword in ["anyOf", "oneOf"] {
        if let Some(schemas) = schema.get(keyword).and_then(Value::as_array)
            && !schemas
                .iter()
                .any(|schema| accepts(schema, value) == Some(true))
        {
            for schema in schemas {
                let mut candidate = value.clone();
                coerce(&mut candidate, schema);
                if accepts(schema, &candidate) == Some(true) {
                    *value = candidate;
                    break;
                }
            }
        }
    }
    let types: Vec<_> = match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    if !types.iter().any(|kind| matches_type(value, kind)) {
        for kind in types {
            if let Some(converted) = primitive(value, kind) {
                *value = converted;
                break;
            }
        }
    }
    match value {
        Value::Object(object) => {
            let properties = schema.get("properties").and_then(Value::as_object);
            for (name, value) in object {
                let property = properties
                    .and_then(|properties| properties.get(name))
                    .or_else(|| {
                        schema
                            .get("additionalProperties")
                            .filter(|schema| schema.is_object())
                    });
                if let Some(property) = property {
                    coerce(value, property);
                }
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                if let Some(schema) = item_schema(schema, index) {
                    coerce(item, schema);
                }
            }
        }
        _ => {}
    }
}

fn item_schema(schema: &Value, index: usize) -> Option<&Value> {
    let items = schema.get("items")?;
    if let Value::Array(items) = items {
        items.get(index)
    } else {
        Some(items)
    }
}

fn matches_type(value: &Value, kind: &str) -> bool {
    match kind {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "number" => value.is_number(),
        "integer" => value.as_f64().is_some_and(|number| number.fract() == 0.0),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

fn primitive(value: &Value, kind: &str) -> Option<Value> {
    // Required nulls stay intact so validation rejects them unless explicitly nullable.
    match (value, kind) {
        (Value::String(text), "number" | "integer") => {
            let number = text
                .trim()
                .parse::<Number>()
                .ok()
                .or_else(|| text.trim().parse::<f64>().ok().and_then(Number::from_f64))?;
            let converted = Value::Number(number);
            matches_type(&converted, kind).then_some(converted)
        }
        (Value::String(text), "boolean") => match text.as_str() {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },
        (Value::Number(number), "string") => Some(Value::String(if number.is_f64() {
            number.as_f64()?.to_string()
        } else {
            number.to_string()
        })),
        (Value::Bool(boolean), "string") => Some(Value::String(boolean.to_string())),
        (Value::Bool(boolean), "number" | "integer") => {
            Some(Value::Number(Number::from(u8::from(*boolean))))
        }
        (Value::Number(number), "boolean") => match number.as_f64()? {
            0.0 => Some(Value::Bool(false)),
            1.0 => Some(Value::Bool(true)),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::json;
    use tiechui_model::ToolSpec;
    use tiechui_tools::{Concurrency, Tool, ToolContext, ToolError, ToolOutput};

    use super::*;

    struct SchemaTool(ToolSpec);

    #[async_trait]
    impl Tool for SchemaTool {
        fn spec(&self) -> &ToolSpec {
            &self.0
        }
        fn concurrency(&self, _: &Value) -> Concurrency {
            Concurrency::Shared
        }
        async fn run(&self, _: ToolCall, _: ToolContext) -> Result<ToolOutput, ToolError> {
            unreachable!("validation must not execute tools")
        }
    }

    fn arguments(schema: Value, arguments: Value) -> Result<Value, String> {
        let mut registry = ToolRegistry::default();
        registry
            .register(Arc::new(SchemaTool(ToolSpec {
                name: "probe".into(),
                description: String::new(),
                input_schema: schema,
            })))
            .unwrap();
        let call = ToolCall {
            id: "test".into(),
            name: "probe".into(),
            arguments,
            opaque: None,
        };
        let original = call.arguments.clone();
        let result = validate(&registry, &call);
        assert_eq!(call.arguments, original);
        result
    }

    fn property(kind: &str, required: bool) -> Value {
        json!({"type":"object", "properties":{"value":{"type":kind}}, "required":if required { vec!["value"] } else { vec![] }})
    }

    #[test]
    fn optional_null_is_dropped_but_nullable_property_is_preserved() {
        assert_eq!(
            arguments(property("string", false), json!({"value":null})).unwrap(),
            json!({})
        );
        let schema = json!({"type":"object", "properties":{"value":{"type":["string","null"]}}});
        assert_eq!(
            arguments(schema, json!({"value":null})).unwrap(),
            json!({"value":null})
        );
    }

    #[test]
    fn required_null_is_rejected_unless_explicitly_nullable() {
        let error = arguments(property("integer", true), json!({"value":null})).unwrap_err();
        assert_eq!(
            error,
            "Validation failed for tool \"probe\":\n  - value: must be integer\n\nReceived arguments:\n{\n  \"value\": null\n}"
        );
        let schema = json!({"type":"object", "properties":{"value":{"type":["integer","null"]}}, "required":["value"]});
        assert_eq!(
            arguments(schema, json!({"value":null})).unwrap(),
            json!({"value":null})
        );
    }

    #[test]
    fn numeric_string_is_accepted_for_integer() {
        assert_eq!(
            arguments(property("integer", true), json!({"value":"5"})).unwrap(),
            json!({"value":5})
        );
        assert_eq!(
            arguments(property("number", true), json!({"value":"5.5"})).unwrap(),
            json!({"value":5.5})
        );
    }

    #[test]
    fn boolean_strings_are_converted() {
        for (text, expected) in [("true", true), ("false", false)] {
            assert_eq!(
                arguments(property("boolean", true), json!({"value":text})).unwrap(),
                json!({"value":expected})
            );
        }
    }

    #[test]
    fn nested_objects_and_array_items_are_normalized_and_converted() {
        let schema = json!({"type":"object", "properties":{
            "nested":{"type":"object", "properties":{"count":{"type":"integer"}, "optional":{"type":"string"}}},
            "items":{"type":"array", "items":{"type":"object", "properties":{"enabled":{"type":"boolean"}, "optional":{"type":"number"}}}},
            "labels":{"type":"array", "items":{"type":"string"}}
        }});
        let input = json!({"nested":{"count":"5","optional":null},"items":[{"enabled":"true","optional":null},{"enabled":"false"}],"labels":[5,true,false]});
        let expected = json!({"nested":{"count":5},"items":[{"enabled":true},{"enabled":false}],"labels":["5","true","false"]});
        assert_eq!(arguments(schema, input).unwrap(), expected);
    }

    #[test]
    fn invalid_integer_keeps_original_arguments_in_error() {
        let error = arguments(property("integer", true), json!({"value":"abc"})).unwrap_err();
        assert_eq!(
            error,
            "Validation failed for tool \"probe\":\n  - value: must be integer\n\nReceived arguments:\n{\n  \"value\": \"abc\"\n}"
        );
    }

    #[test]
    fn coercion_matches_fixtures() {
        let errors: Value =
            serde_json::from_str(include_str!("../tests/fixtures/tool-errors.json")).unwrap();
        for fixture in errors["coercion"].as_array().unwrap() {
            let result = arguments(fixture["schema"].clone(), fixture["arguments"].clone());
            if let Some(error) = fixture.get("error") {
                assert_eq!(result.unwrap_err(), error.as_str().unwrap());
            } else {
                assert_eq!(result.unwrap(), fixture["converted"]);
            }
        }
    }
}

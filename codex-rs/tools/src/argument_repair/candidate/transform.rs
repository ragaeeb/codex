use super::super::ArgumentRepairRule;
use super::super::pointer::JsonPointer;
use crate::JsonSchema;
use crate::JsonSchemaPrimitiveType;
use crate::JsonSchemaType;
use serde_json::Number as JsonNumber;
use serde_json::Value as JsonValue;

pub(super) fn type_replacements(
    expected_type: JsonSchemaPrimitiveType,
    items: Option<&JsonSchema>,
    value: &JsonValue,
) -> Vec<(JsonValue, ArgumentRepairRule)> {
    match expected_type {
        JsonSchemaPrimitiveType::Array => array_replacements(items, value),
        JsonSchemaPrimitiveType::Object => decoded_container(value, JsonValue::is_object)
            .into_iter()
            .map(|value| (value, ArgumentRepairRule::StringifiedObjectDecoded))
            .collect(),
        JsonSchemaPrimitiveType::Number | JsonSchemaPrimitiveType::Integer => value
            .as_str()
            .and_then(|value| parse_number(value, expected_type))
            .map(|value| {
                vec![(
                    JsonValue::Number(value),
                    ArgumentRepairRule::NumericStringTyped,
                )]
            })
            .unwrap_or_default(),
        JsonSchemaPrimitiveType::Boolean => value
            .as_str()
            .and_then(parse_boolean)
            .map(|value| {
                vec![(
                    JsonValue::Bool(value),
                    ArgumentRepairRule::BooleanStringTyped,
                )]
            })
            .unwrap_or_default(),
        JsonSchemaPrimitiveType::String | JsonSchemaPrimitiveType::Null => Vec::new(),
    }
}

fn array_replacements(
    items: Option<&JsonSchema>,
    value: &JsonValue,
) -> Vec<(JsonValue, ArgumentRepairRule)> {
    let mut replacements = decoded_container(value, JsonValue::is_array)
        .into_iter()
        .map(|value| (value, ArgumentRepairRule::StringifiedArrayDecoded))
        .collect::<Vec<_>>();
    if !value.is_array() && !value.is_object() && items.is_some() {
        replacements.push((
            JsonValue::Array(vec![value.clone()]),
            ArgumentRepairRule::ScalarWrappedInArray,
        ));
    }
    replacements
}

fn decoded_container(
    value: &JsonValue,
    required_shape: fn(&JsonValue) -> bool,
) -> Option<JsonValue> {
    let decoded = serde_json::from_str::<JsonValue>(value.as_str()?).ok()?;
    required_shape(&decoded).then_some(decoded)
}

fn parse_boolean(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn parse_number(value: &str, expected_type: JsonSchemaPrimitiveType) -> Option<JsonNumber> {
    if value.trim() != value {
        return None;
    }
    let parsed = serde_json::from_str::<JsonValue>(value).ok()?;
    let number = parsed.as_number()?;
    let canonical = match expected_type {
        JsonSchemaPrimitiveType::Integer
            if !value.contains('e')
                && !value.contains('E')
                && is_mathematical_integer(number)
                && (number.as_i64().is_some() || number.as_u64().is_some()) =>
        {
            number.clone()
        }
        JsonSchemaPrimitiveType::Number => {
            let float = number.as_f64()?;
            let canonical = JsonNumber::from_f64(float)?;
            (decimal_signature(value)? == decimal_signature(&canonical.to_string())?)
                .then_some(canonical)?
        }
        JsonSchemaPrimitiveType::String
        | JsonSchemaPrimitiveType::Boolean
        | JsonSchemaPrimitiveType::Integer
        | JsonSchemaPrimitiveType::Object
        | JsonSchemaPrimitiveType::Array
        | JsonSchemaPrimitiveType::Null => return None,
    };
    Some(canonical)
}

fn is_mathematical_integer(number: &JsonNumber) -> bool {
    decimal_signature(&number.to_string()).is_some_and(|(_, _, exponent)| exponent >= 0)
}

fn decimal_signature(value: &str) -> Option<(bool, String, i64)> {
    let (negative, value) = value
        .strip_prefix('-')
        .map_or((false, value), |value| (true, value));
    let exponent_index = value.find('e').or_else(|| value.find('E'));
    let (mantissa, exponent) = match exponent_index {
        Some(index) => (&value[..index], value[index + 1..].parse::<i64>().ok()?),
        None => (value, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }

    let digits = format!("{whole}{fraction}");
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Some((false, "0".to_string(), 0));
    }
    let trailing_zeros = digits
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'0')
        .count();
    let coefficient = &digits[..digits.len() - trailing_zeros];
    let exponent = exponent
        .checked_sub(i64::try_from(fraction.len()).ok()?)?
        .checked_add(i64::try_from(trailing_zeros).ok()?)?;
    Some((negative, coefficient.to_string(), exponent))
}

pub(super) fn unwrap_markdown_path(value: &str) -> Option<&str> {
    if let Some(inner) = value
        .strip_prefix('`')
        .and_then(|value| value.strip_suffix('`'))
        && !inner.is_empty()
        && !inner
            .chars()
            .any(|character| matches!(character, '`' | '\r' | '\n'))
    {
        return Some(inner);
    }
    let inner = value.strip_prefix("```\n")?.strip_suffix("\n```")?;
    (!inner.is_empty()
        && !inner
            .chars()
            .any(|character| matches!(character, '`' | '\r' | '\n')))
    .then_some(inner)
}

pub(super) fn replace_at_path(
    value: &mut JsonValue,
    path: &JsonPointer,
    replacement: JsonValue,
) -> bool {
    let Some(target) = value_at_path_mut(value, path) else {
        return false;
    };
    *target = replacement;
    true
}

pub(super) fn remove_property_at_path(value: &mut JsonValue, path: &JsonPointer) -> bool {
    let Some((property, parent_tokens)) = path.tokens().split_last() else {
        return false;
    };
    let parent_path = JsonPointer::from_tokens(parent_tokens.to_vec());
    let Some(JsonValue::Object(parent)) = value_at_path_mut(value, &parent_path) else {
        return false;
    };
    parent.remove(property).is_some()
}

fn value_at_path_mut<'a>(
    value: &'a mut JsonValue,
    path: &JsonPointer,
) -> Option<&'a mut JsonValue> {
    let mut value = value;
    for token in path.tokens() {
        value = match value {
            JsonValue::Object(object) => object.get_mut(token)?,
            JsonValue::Array(array) => array.get_mut(token.parse::<usize>().ok()?)?,
            JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) | JsonValue::String(_) => {
                return None;
            }
        };
    }
    Some(value)
}

pub(super) fn primitive_types(schema_type: &JsonSchemaType) -> Vec<JsonSchemaPrimitiveType> {
    let types = match schema_type {
        JsonSchemaType::Single(schema_type) => vec![*schema_type],
        JsonSchemaType::Multiple(schema_types) => schema_types.clone(),
    };
    let mut unique = Vec::new();
    for schema_type in types {
        if !unique.contains(&schema_type) {
            unique.push(schema_type);
        }
    }
    unique
}

pub(super) fn matches_schema_type(schema_type: &JsonSchemaType, value: &JsonValue) -> bool {
    primitive_types(schema_type)
        .into_iter()
        .any(|schema_type| match schema_type {
            JsonSchemaPrimitiveType::String => value.is_string(),
            JsonSchemaPrimitiveType::Number => value.is_number(),
            JsonSchemaPrimitiveType::Boolean => value.is_boolean(),
            JsonSchemaPrimitiveType::Integer => {
                value.as_number().is_some_and(is_mathematical_integer)
            }
            JsonSchemaPrimitiveType::Object => value.is_object(),
            JsonSchemaPrimitiveType::Array => value.is_array(),
            JsonSchemaPrimitiveType::Null => value.is_null(),
        })
}

pub(super) fn type_rank(schema_type: JsonSchemaPrimitiveType) -> u8 {
    match schema_type {
        JsonSchemaPrimitiveType::String => 0,
        JsonSchemaPrimitiveType::Number => 1,
        JsonSchemaPrimitiveType::Boolean => 2,
        JsonSchemaPrimitiveType::Integer => 3,
        JsonSchemaPrimitiveType::Object => 4,
        JsonSchemaPrimitiveType::Array => 5,
        JsonSchemaPrimitiveType::Null => 6,
    }
}

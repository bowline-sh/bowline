// Generic, data-driven validator for hosted wire values. It walks the generated
// canonical schema (`HOSTED_WIRE_SCHEMA`) so Rust enforces the same semantic
// constraints as the TS/Convex guards — effective number bounds, string length,
// timestamp format, collection/map bounds, presence/null and unknown-field
// policy — that Serde's structural parsing alone does not. There is no
// per-endpoint parser and no second copy of the constraints: the schema is the
// single source.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use serde_json::Value as JsonValue;

use super::generated::HOSTED_WIRE_SCHEMA;

/// A wire-contract violation identifying the field path that failed. It never
/// carries the offending value, so surfacing it cannot leak secret payloads.
#[derive(Debug)]
pub(super) struct WireContractViolation {
    path: String,
}

impl WireContractViolation {
    pub(super) fn path(&self) -> &str {
        &self.path
    }
}

fn schema_registry() -> &'static BTreeMap<String, JsonValue> {
    static REGISTRY: OnceLock<BTreeMap<String, JsonValue>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        serde_json::from_str(HOSTED_WIRE_SCHEMA)
            .expect("generated hosted wire schema is valid JSON")
    })
}

/// Validates a wire value against the named generated declaration.
pub(super) fn validate_wire_value(
    schema_name: &str,
    value: &JsonValue,
) -> Result<(), WireContractViolation> {
    validate_declaration(schema_registry(), schema_name, value, "$")
}

fn violation(path: &str) -> WireContractViolation {
    WireContractViolation {
        path: path.to_string(),
    }
}

fn str_field<'a>(schema: &'a JsonValue, key: &str) -> Option<&'a str> {
    schema.get(key).and_then(JsonValue::as_str)
}

fn validate_declaration(
    registry: &BTreeMap<String, JsonValue>,
    name: &str,
    value: &JsonValue,
    path: &str,
) -> Result<(), WireContractViolation> {
    let Some(schema) = registry.get(name) else {
        return Err(violation(path));
    };
    match str_field(schema, "kind") {
        Some("alias") => validate_type(registry, &schema["type"], value, path),
        Some("enum") => validate_enum(schema, value, path),
        Some("record") => validate_record(registry, schema, value, path),
        Some("tagged-union") => validate_union(registry, schema, value, path),
        _ => Err(violation(path)),
    }
}

fn validate_type(
    registry: &BTreeMap<String, JsonValue>,
    type_schema: &JsonValue,
    value: &JsonValue,
    path: &str,
) -> Result<(), WireContractViolation> {
    match str_field(type_schema, "kind") {
        Some("boolean") => require(value.is_boolean(), path),
        Some("number") => validate_number(type_schema, value, path),
        Some("string") => validate_string(type_schema, value, path),
        Some("timestamp") => validate_timestamp(value, path),
        // An OpaqueJson value is any JSON value, including null.
        Some("json") => Ok(()),
        Some("literal") => require(value == &type_schema["value"], path),
        Some("reference") => validate_declaration(
            registry,
            str_field(type_schema, "name").unwrap_or(""),
            value,
            path,
        ),
        Some("array") => validate_array(registry, type_schema, value, path),
        Some("map") => validate_map(registry, type_schema, value, path),
        Some("nullable") => {
            if value.is_null() {
                Ok(())
            } else {
                validate_type(registry, &type_schema["value"], value, path)
            }
        }
        _ => Err(violation(path)),
    }
}

fn require(ok: bool, path: &str) -> Result<(), WireContractViolation> {
    if ok { Ok(()) } else { Err(violation(path)) }
}

fn validate_number(
    type_schema: &JsonValue,
    value: &JsonValue,
    path: &str,
) -> Result<(), WireContractViolation> {
    let Some(number) = value.as_f64().filter(|number| number.is_finite()) else {
        return Err(violation(path));
    };
    if type_schema.get("integer").and_then(JsonValue::as_bool) == Some(true)
        && number.fract() != 0.0
    {
        return Err(violation(path));
    }
    if let Some(minimum) = type_schema.get("minimum").and_then(JsonValue::as_f64) {
        require(number >= minimum, path)?;
    }
    if let Some(maximum) = type_schema.get("maximum").and_then(JsonValue::as_f64) {
        require(number <= maximum, path)?;
    }
    Ok(())
}

fn validate_string(
    type_schema: &JsonValue,
    value: &JsonValue,
    path: &str,
) -> Result<(), WireContractViolation> {
    let Some(text) = value.as_str() else {
        return Err(violation(path));
    };
    if let Some(max_length) = type_schema.get("maxLength").and_then(JsonValue::as_u64) {
        // Match the guards' `String.length`, which counts UTF-16 code units.
        require((text.encode_utf16().count() as u64) <= max_length, path)?;
    }
    Ok(())
}

fn validate_timestamp(value: &JsonValue, path: &str) -> Result<(), WireContractViolation> {
    let Some(text) = value.as_str() else {
        return Err(violation(path));
    };
    require(is_canonical_rfc3339(text), path)
}

// Enforces Bowline's explicit canonical timestamp policy directly, identical to
// the generated TS/Convex `isWireRfc3339` guard, instead of delegating to a
// parser whose leniencies (arbitrary date/time separators, leap seconds) would
// silently diverge from the JS guards. Policy: `YYYY-MM-DD`, a 'T'/'t'/space
// separator, `hh:mm:ss`, an optional `.` plus one-or-more fraction digits, and a
// 'Z'/'z' or +/-(00-23):(00-59) offset; real calendar dates with leap-year day
// counts; hour 00-23, minute/second 00-59 (no leap seconds, no 24:00).
fn is_canonical_rfc3339(text: &str) -> bool {
    if !text.is_ascii() {
        return false;
    }
    let bytes = text.as_bytes();
    // "YYYY-MM-DDThh:mm:ss" is 19 bytes; the shortest valid value ends in 'Z'.
    if bytes.len() < 20 {
        return false;
    }
    let digit = |byte: u8| byte.is_ascii_digit();
    let two = |tens: u8, ones: u8| u32::from(tens - b'0') * 10 + u32::from(ones - b'0');
    let skeleton_ok = digit(bytes[0])
        && digit(bytes[1])
        && digit(bytes[2])
        && digit(bytes[3])
        && bytes[4] == b'-'
        && digit(bytes[5])
        && digit(bytes[6])
        && bytes[7] == b'-'
        && digit(bytes[8])
        && digit(bytes[9])
        && matches!(bytes[10], b'T' | b't' | b' ')
        && digit(bytes[11])
        && digit(bytes[12])
        && bytes[13] == b':'
        && digit(bytes[14])
        && digit(bytes[15])
        && bytes[16] == b':'
        && digit(bytes[17])
        && digit(bytes[18]);
    if !skeleton_ok {
        return false;
    }
    let year = two(bytes[0], bytes[1]) * 100 + two(bytes[2], bytes[3]);
    let month = two(bytes[5], bytes[6]);
    let day = two(bytes[8], bytes[9]);
    let hour = two(bytes[11], bytes[12]);
    let minute = two(bytes[14], bytes[15]);
    let second = two(bytes[17], bytes[18]);
    if !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 59 {
        return false;
    }
    let leap_year = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let month_days = [
        31,
        if leap_year { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day < 1 || day > month_days[(month - 1) as usize] {
        return false;
    }
    // Optional fractional seconds: '.' then one or more digits.
    let mut index = 19;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let fraction_start = index;
        while bytes.get(index).is_some_and(|byte| byte.is_ascii_digit()) {
            index += 1;
        }
        if index == fraction_start {
            return false;
        }
    }
    // Offset: 'Z'/'z' or +/-hh:mm within range, with nothing trailing.
    match bytes.get(index..) {
        Some([b'Z' | b'z']) => true,
        Some([sign, h1, h2, b':', m1, m2])
            if matches!(sign, b'+' | b'-')
                && digit(*h1)
                && digit(*h2)
                && digit(*m1)
                && digit(*m2) =>
        {
            two(*h1, *h2) <= 23 && two(*m1, *m2) <= 59
        }
        _ => false,
    }
}

fn validate_array(
    registry: &BTreeMap<String, JsonValue>,
    type_schema: &JsonValue,
    value: &JsonValue,
    path: &str,
) -> Result<(), WireContractViolation> {
    let Some(items) = value.as_array() else {
        return Err(violation(path));
    };
    if let Some(max_items) = type_schema.get("maxItems").and_then(JsonValue::as_u64) {
        require((items.len() as u64) <= max_items, path)?;
    }
    for (index, item) in items.iter().enumerate() {
        validate_type(
            registry,
            &type_schema["items"],
            item,
            &format!("{path}[{index}]"),
        )?;
    }
    Ok(())
}

fn validate_map(
    registry: &BTreeMap<String, JsonValue>,
    type_schema: &JsonValue,
    value: &JsonValue,
    path: &str,
) -> Result<(), WireContractViolation> {
    let Some(entries) = value.as_object() else {
        return Err(violation(path));
    };
    if let Some(max_entries) = type_schema.get("maxEntries").and_then(JsonValue::as_u64) {
        require((entries.len() as u64) <= max_entries, path)?;
    }
    let key_max_length = type_schema.get("keyMaxLength").and_then(JsonValue::as_u64);
    for (key, item) in entries {
        let entry_path = format!("{path}.{key}");
        if let Some(max_length) = key_max_length {
            require(
                (key.encode_utf16().count() as u64) <= max_length,
                &entry_path,
            )?;
        }
        validate_type(registry, &type_schema["values"], item, &entry_path)?;
    }
    Ok(())
}

fn validate_enum(
    schema: &JsonValue,
    value: &JsonValue,
    path: &str,
) -> Result<(), WireContractViolation> {
    let Some(text) = value.as_str() else {
        return Err(violation(path));
    };
    if str_field(schema, "unknownPolicy") == Some("preserve") {
        return Ok(());
    }
    let known = schema
        .get("known")
        .and_then(JsonValue::as_array)
        .is_some_and(|values| values.iter().any(|known| known.as_str() == Some(text)));
    require(known, path)
}

fn validate_record(
    registry: &BTreeMap<String, JsonValue>,
    schema: &JsonValue,
    value: &JsonValue,
    path: &str,
) -> Result<(), WireContractViolation> {
    let Some(object) = value.as_object() else {
        return Err(violation(path));
    };
    let empty = Vec::new();
    let fields = schema
        .get("fields")
        .and_then(JsonValue::as_array)
        .unwrap_or(&empty);
    let mut known_keys = BTreeSet::new();
    for field in fields {
        let wire_name = str_field(field, "wireName")
            .or_else(|| str_field(field, "name"))
            .unwrap_or_default();
        known_keys.insert(wire_name);
        let field_path = format!("{path}.{wire_name}");
        match object.get(wire_name) {
            // Absent: required fails; optional is allowed. Explicit null is
            // present and is checked against the field type (only a nullable
            // type accepts it).
            None => {
                if field.get("required").and_then(JsonValue::as_bool) == Some(true) {
                    return Err(violation(&field_path));
                }
            }
            Some(field_value) => {
                validate_type(registry, &field["type"], field_value, &field_path)?;
            }
        }
    }
    if str_field(schema, "unknownFields") != Some("accept") {
        for key in object.keys() {
            if !known_keys.contains(key.as_str()) {
                return Err(violation(&format!("{path}.{key}")));
            }
        }
    }
    Ok(())
}

fn validate_union(
    registry: &BTreeMap<String, JsonValue>,
    schema: &JsonValue,
    value: &JsonValue,
    path: &str,
) -> Result<(), WireContractViolation> {
    let Some(object) = value.as_object() else {
        return Err(violation(path));
    };
    let discriminator = str_field(schema, "discriminator").unwrap_or_default();
    let Some(tag) = object.get(discriminator).and_then(JsonValue::as_str) else {
        return Err(violation(path));
    };
    let variant = schema
        .get("variants")
        .and_then(JsonValue::as_array)
        .and_then(|variants| {
            variants
                .iter()
                .find(|variant| str_field(variant, "tag") == Some(tag))
        });
    match variant {
        Some(variant) => validate_type(registry, &variant["type"], value, path),
        None => require(str_field(schema, "unknownPolicy") == Some("preserve"), path),
    }
}

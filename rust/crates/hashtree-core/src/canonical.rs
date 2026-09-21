use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) fn serialize_metadata<S: Serializer>(
    metadata: &Option<BTreeMap<String, Value>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match metadata {
        None => serializer.serialize_none(),
        Some(metadata) => {
            let mut map = serializer.serialize_map(Some(metadata.len()))?;
            for (key, value) in metadata {
                map.serialize_entry(key, &CanonicalValue(value))?;
            }
            map.end()
        }
    }
}

struct CanonicalValue<'a>(&'a Value);

impl Serialize for CanonicalValue<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Value::Null => serializer.serialize_none(),
            Value::Bool(value) => serializer.serialize_bool(*value),
            Value::String(value) => serializer.serialize_str(value),
            Value::Number(value) => {
                if let Some(value) = value.as_u64() {
                    return serializer.serialize_u64(value);
                }
                if let Some(value) = value.as_i64() {
                    return serializer.serialize_i64(value);
                }
                let value = value.as_f64().ok_or_else(|| {
                    serde::ser::Error::custom("Metadata number is not representable")
                })?;
                if !value.is_finite() {
                    return Err(serde::ser::Error::custom("Metadata numbers must be finite"));
                }
                // Integral binary64 values use the same bytes as integer inputs, including -0.
                if value.fract() == 0.0 {
                    if (0.0..18446744073709551616.0).contains(&value) {
                        return serializer.serialize_u64(value as u64);
                    }
                    if (-9223372036854775808.0..0.0).contains(&value) {
                        return serializer.serialize_i64(value as i64);
                    }
                }
                serializer.serialize_f64(value)
            }
            Value::Array(values) => {
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(&CanonicalValue(value))?;
                }
                sequence.end()
            }
            Value::Object(values) => {
                // Explicitly sort even when serde_json's preserve_order feature is enabled.
                let sorted: BTreeMap<_, _> = values.iter().collect();
                let mut map = serializer.serialize_map(Some(sorted.len()))?;
                for (key, value) in sorted {
                    map.serialize_entry(key, &CanonicalValue(value))?;
                }
                map.end()
            }
        }
    }
}

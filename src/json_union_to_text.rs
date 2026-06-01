use std::any::Any;
use std::fmt::Write as _;
use std::sync::Arc;

use datafusion::arrow::array::{Array, ArrayRef, StringViewBuilder, UnionArray};
use datafusion::arrow::datatypes::DataType;
use datafusion::common::{exec_err, plan_err, Result as DataFusionResult};
use datafusion::logical_expr::{ColumnarValue, ScalarFunctionArgs, ScalarUDFImpl, Signature, Volatility};

use crate::common_macros::make_udf_function;
use crate::common_union::{is_json_union, JsonUnionEncoder, JsonUnionValue, JSON_UNION_DATA_TYPE};

make_udf_function!(
    JsonUnionToText,
    json_union_to_text,
    json_union,
    "Flatten a JSON union value (produced by `json_get`) into its canonical JSON text"
);

/// Flattens the heterogeneous JSON union that `json_get` produces into a single
/// `Utf8View` column of canonical JSON text: scalars render as `true` / `42` /
/// `1.5`, strings are JSON-quoted and escaped, and array/object arms (already raw
/// JSON text) pass through. A JSON `null` arm becomes a SQL `NULL`.
///
/// Useful when a JSON-union column must be materialized somewhere that can't
/// represent an Arrow `Union` — e.g. the Parquet writer, which rejects unions
/// (`arrow_to_parquet_schema` panics with "See ARROW-8817.").
#[derive(Debug, PartialEq, Eq, Hash)]
pub(super) struct JsonUnionToText {
    signature: Signature,
    aliases: [String; 1],
}

impl Default for JsonUnionToText {
    fn default() -> Self {
        Self {
            // Exactly the JSON union — any other argument type is a planning error.
            signature: Signature::exact(vec![JSON_UNION_DATA_TYPE.clone()], Volatility::Immutable),
            aliases: ["json_union_to_text".to_string()],
        }
    }
}

impl ScalarUDFImpl for JsonUnionToText {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        self.aliases[0].as_str()
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, arg_types: &[DataType]) -> DataFusionResult<DataType> {
        match arg_types {
            [t] if is_json_union(t) => Ok(DataType::Utf8View),
            _ => plan_err!("json_union_to_text expects a single JSON-union argument, got {arg_types:?}"),
        }
    }

    fn invoke_with_args(&self, args: ScalarFunctionArgs) -> DataFusionResult<ColumnarValue> {
        let Some(arg) = args.args.into_iter().next() else {
            return exec_err!("json_union_to_text expects one argument");
        };
        let array = arg.into_array(args.number_rows)?;
        Ok(ColumnarValue::Array(json_union_to_text_array(&array)?))
    }

    fn aliases(&self) -> &[String] {
        &self.aliases
    }
}

/// Encode a JSON-union array into a `Utf8View` array of canonical JSON text.
fn json_union_to_text_array(array: &ArrayRef) -> DataFusionResult<ArrayRef> {
    let Some(union) = array.as_any().downcast_ref::<UnionArray>() else {
        return exec_err!("json_union_to_text expects a UnionArray argument");
    };
    let Some(encoder) = JsonUnionEncoder::from_union(union.clone()) else {
        return exec_err!("json_union_to_text argument is not the JSON union type");
    };

    let mut builder = StringViewBuilder::with_capacity(encoder.len());
    let mut scratch = String::new();
    for idx in 0..encoder.len() {
        match encoder.get_value(idx) {
            JsonUnionValue::JsonNull => builder.append_null(),
            JsonUnionValue::Bool(b) => builder.append_value(if b { "true" } else { "false" }),
            JsonUnionValue::Int(i) => {
                scratch.clear();
                let _ = write!(scratch, "{i}");
                builder.append_value(&scratch);
            }
            JsonUnionValue::Float(f) => {
                scratch.clear();
                let _ = write!(scratch, "{f}");
                builder.append_value(&scratch);
            }
            // A bare string scalar must be JSON-quoted and escaped.
            JsonUnionValue::Str(s) => {
                scratch.clear();
                push_json_string(&mut scratch, s);
                builder.append_value(&scratch);
            }
            // The array/object arms already hold valid JSON text — pass through.
            JsonUnionValue::Array(s) | JsonUnionValue::Object(s) => builder.append_value(s),
        }
    }
    Ok(Arc::new(builder.finish()))
}

/// Append `s` to `out` as a JSON string literal (quoted + escaped), matching the
/// escaping `serde_json` emits: `"`, `\`, and the C0 control characters; bytes
/// `>= 0x20` (incl. non-ASCII) are written verbatim.
fn push_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common_union::{JsonUnion, JsonUnionField};
    use datafusion::arrow::array::StringViewArray;

    #[test]
    fn flattens_each_arm_to_json_text() {
        let union = JsonUnion::from_iter(vec![
            Some(JsonUnionField::JsonNull),
            Some(JsonUnionField::Bool(true)),
            Some(JsonUnionField::Int(42)),
            Some(JsonUnionField::Float(1.5)),
            Some(JsonUnionField::Str("foo\"bar\n".to_string())),
            Some(JsonUnionField::Array("[1,2]".to_string())),
            Some(JsonUnionField::Object(r#"{"a":1}"#.to_string())),
            None,
        ]);
        let array: ArrayRef = Arc::new(UnionArray::try_from(union).unwrap());

        let out = json_union_to_text_array(&array).unwrap();
        let strings = out.as_any().downcast_ref::<StringViewArray>().unwrap();
        let got: Vec<Option<&str>> = (0..strings.len())
            .map(|i| (!strings.is_null(i)).then(|| strings.value(i)))
            .collect();
        assert_eq!(
            got,
            vec![
                None,                      // JsonNull
                Some("true"),              // Bool
                Some("42"),                // Int
                Some("1.5"),               // Float
                Some("\"foo\\\"bar\\n\""), // Str: JSON-quoted + escaped (incl. control char)
                Some("[1,2]"),             // Array (passthrough)
                Some(r#"{"a":1}"#),        // Object (passthrough)
                None,                      // None
            ]
        );
    }
}

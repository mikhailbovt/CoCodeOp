use std::{any::TypeId, cell::RefCell, collections::HashMap, sync::Arc};

use rmcp::model::JsonObject;
use schemars::{JsonSchema, generate::SchemaSettings};
use serde_json::Value;

/// Publish explicit types for nested parameters. Some provider tool parsers
/// classify reference-only parameters as strings before the client sees them.
pub(super) fn typed_input<T: JsonSchema + 'static>() -> Arc<JsonObject> {
    thread_local! {
        static SCHEMAS: RefCell<HashMap<TypeId, Arc<JsonObject>>> = RefCell::new(HashMap::new());
    }
    SCHEMAS.with(|cache| {
        cache
            .borrow_mut()
            .entry(TypeId::of::<T>())
            .or_insert_with(|| {
                let generator = SchemaSettings::draft2020_12()
                    .with(|settings| {
                        settings.inline_subschemas = true;
                        settings.meta_schema = None;
                    })
                    .into_generator();
                let mut schema = generator.into_root_schema_for::<T>().to_value();
                mark_object_unions(&mut schema);
                let Value::Object(mut schema) = schema else {
                    unreachable!("MCP inputs are object schemas");
                };
                schema.remove("title");
                Arc::new(schema)
            })
            .clone()
    })
}

fn mark_object_unions(schema: &mut Value) {
    match schema {
        Value::Array(entries) => entries.iter_mut().for_each(mark_object_unions),
        Value::Object(fields) => {
            for field in fields.values_mut() {
                mark_object_unions(field);
            }
            if !fields.contains_key("type")
                && ["oneOf", "anyOf"].iter().any(|kind| {
                    fields
                        .get(*kind)
                        .and_then(Value::as_array)
                        .is_some_and(|branches| {
                            !branches.is_empty()
                                && branches.iter().all(|branch| branch["type"] == "object")
                        })
                })
            {
                fields.insert("type".into(), Value::String("object".into()));
            }
        }
        _ => {}
    }
}

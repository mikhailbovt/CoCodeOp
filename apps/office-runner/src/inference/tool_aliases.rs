use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::ProviderApi;

#[derive(Default)]
pub(super) struct ToolAliases(BTreeMap<String, (String, String)>);

fn local_tool(tool: &Value) -> bool {
    matches!(
        tool.get("type").and_then(Value::as_str),
        Some("function" | "custom")
    )
}

impl ToolAliases {
    pub fn prepare(
        api: ProviderApi,
        flatten: bool,
        request: &mut Map<String, Value>,
    ) -> Option<Self> {
        let mut aliases = Self::default();
        let Some(tools) = request.get_mut("tools").filter(|value| !value.is_null()) else {
            return Some(aliases);
        };
        let tools = tools.as_array_mut()?;
        if tools.len() > 128 {
            return None;
        }
        let mut count = 0;
        let mut flattened = Vec::new();
        let mut names = BTreeSet::new();
        for tool in tools.iter() {
            if local_tool(tool) {
                count += 1;
                if flatten {
                    let name = identifier(tool, "name")?;
                    if !names.insert(name.to_owned()) {
                        return None;
                    }
                    flattened.push(tool.clone());
                }
                continue;
            }
            if !matches!(api, ProviderApi::Responses)
                || tool.get("type").and_then(Value::as_str) != Some("namespace")
            {
                return None;
            }
            let namespace = identifier(tool, "name")?;
            let children = tool.get("tools")?.as_array()?;
            count += children.len();
            if children.is_empty() || count > 128 || !children.iter().all(local_tool) {
                return None;
            }
            if flatten {
                for child in children {
                    let name = identifier(child, "name")?;
                    let alias = alias(namespace, name);
                    if !names.insert(alias.clone()) {
                        return None;
                    }
                    let mut exposed = child.clone();
                    exposed["name"] = json!(&alias);
                    flattened.push(exposed);
                    aliases.0.insert(alias, (namespace.into(), name.into()));
                }
            }
        }
        if count > 128 {
            return None;
        }
        if flatten {
            *tools = flattened;
            if let Some(history) = request.get_mut("input").and_then(Value::as_array_mut) {
                for item in history.iter_mut().filter(|item| is_call(item)) {
                    aliases.flatten_item(item);
                }
            }
            if let Some(choice) = request.get_mut("tool_choice") {
                aliases.flatten_item(choice);
            }
        }
        Some(aliases)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn flatten_item(&self, item: &mut Value) {
        let Some((namespace, name)) = identifier(item, "namespace").zip(identifier(item, "name"))
        else {
            return;
        };
        let alias = alias(namespace, name);
        if self
            .0
            .get(&alias)
            .is_some_and(|(ns, tool)| ns == namespace && tool == name)
            && let Some(fields) = item.as_object_mut()
        {
            fields.remove("namespace");
            fields.insert("name".into(), json!(alias));
        }
    }

    pub fn restore_response(&self, response: &mut Value) {
        if let Some(item) = response.get_mut("item") {
            self.restore_item(item);
        }
        let output = if response.get("response").is_some() {
            response.pointer_mut("/response/output")
        } else {
            response.get_mut("output")
        };
        if let Some(items) = output.and_then(Value::as_array_mut) {
            for item in items {
                self.restore_item(item);
            }
        }
    }

    fn restore_item(&self, item: &mut Value) {
        if !is_call(item) {
            return;
        }
        if let Some((namespace, name)) = item
            .get("name")
            .and_then(Value::as_str)
            .and_then(|alias| self.0.get(alias))
        {
            item["name"] = json!(name);
            item["namespace"] = json!(namespace);
        }
    }
}

fn is_call(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("function_call" | "custom_tool_call")
    )
}

fn identifier<'a>(tool: &'a Value, field: &str) -> Option<&'a str> {
    tool.get(field).and_then(Value::as_str).filter(|name| {
        (1..=128).contains(&name.len())
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    })
}

fn alias(namespace: &str, name: &str) -> String {
    let joined = format!("{}__{name}", namespace.trim_end_matches('_'));
    if joined.len() <= 64 {
        return joined;
    }
    let digest = hex::encode(Sha256::digest(format!("{namespace}\0{name}")));
    format!("{}_{}", &digest[..16], &name[..name.len().min(47)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaces_round_trip_as_function_names_and_cannot_shadow_a_plain_tool() {
        let mut body = json!({
            "tools":[{"type":"namespace","name":"mcp__office","tools":[{"type":"function","name":"office_context"}]}],
            "input":[{"type":"function_call","namespace":"mcp__office","name":"office_context","call_id":"call_1"}],
        });
        let aliases =
            ToolAliases::prepare(ProviderApi::Responses, true, body.as_object_mut().unwrap())
                .unwrap();
        assert_eq!(body["tools"][0]["name"], "mcp__office__office_context");
        assert_eq!(body["input"][0]["name"], "mcp__office__office_context");
        assert!(body["input"][0].get("namespace").is_none());
        let mut event = json!({"item":body["input"][0]});
        aliases.restore_response(&mut event);
        assert_eq!(event["item"]["namespace"], "mcp__office");
        assert_eq!(event["item"]["name"], "office_context");
        assert_eq!(event["item"]["call_id"], "call_1");

        let mut invalid = json!({
            "tools":[{"type":"function","name":"mcp__office__office_context"},
               {"type":"namespace","name":"mcp__office","tools":[{"type":"function","name":"office_context"}]}]
        });
        assert!(
            ToolAliases::prepare(
                ProviderApi::Responses,
                true,
                invalid.as_object_mut().unwrap()
            )
            .is_none()
        );
        invalid["tools"][1]["tools"][0]["type"] = json!("web_search");
        assert!(
            ToolAliases::prepare(
                ProviderApi::Responses,
                false,
                invalid.as_object_mut().unwrap()
            )
            .is_none()
        );
    }
}

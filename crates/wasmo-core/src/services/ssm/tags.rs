use serde_json::{Value, json};

use super::{Parameter, Region, err, require, strings, validation};
use crate::codec::AwsError;
use crate::service::{InputExt, OpResult};

const RESOURCE_TYPES: [&str; 9] = [
    "Document",
    "ManagedInstance",
    "MaintenanceWindow",
    "Parameter",
    "PatchBaseline",
    "OpsItem",
    "OpsMetadata",
    "Automation",
    "Association",
];
const MAX_TAGS: usize = 50;

fn resource<'a>(region: &'a mut Region, input: &Value) -> Result<&'a mut Parameter, AwsError> {
    let kind = require(input, "ResourceType")?;
    if !RESOURCE_TYPES.contains(&kind) {
        return Err(err(
            "InvalidResourceType",
            "The resource type isn't valid. For example, if you are attempting to tag an EC2 instance, the instance must be a registered managed node.",
        ));
    }
    let id = require(input, "ResourceId")?;
    let invalid = || {
        err(
            "InvalidResourceId",
            "The resource ID isn't valid. Verify that you entered the correct ID and try again.",
        )
    };
    if kind != "Parameter" {
        return Err(invalid());
    }
    let name = region.resolve_name(id);
    let name = if region.params.contains_key(&name) {
        name
    } else if let Some(bare) = name.strip_prefix('/').filter(|b| !b.contains('/')) {
        bare.to_string()
    } else {
        format!("/{name}")
    };
    region.params.get_mut(&name).ok_or_else(invalid)
}

pub(crate) fn add(region: &mut Region, input: &Value) -> OpResult {
    let param = resource(region, input)?;
    let tags = input.array_field("Tags");
    if tags.is_empty() {
        return Err(validation(
            "1 validation error detected: Value null at 'tags' failed to satisfy constraint: Member must not be null",
        ));
    }
    let mut merged = param.tags.clone();
    for tag in tags {
        let key = tag
            .str_field("Key")
            .ok_or_else(|| validation("Tag Key is required."))?;
        let value = tag.get("Value").and_then(Value::as_str).unwrap_or_default();
        merged.insert(key.to_string(), value.to_string());
    }
    if merged.len() > MAX_TAGS {
        return Err(err(
            "TooManyTagsError",
            "The Targets parameter includes too many tags. Remove one or more tags and try the command again.",
        ));
    }
    param.tags = merged;
    Ok(json!({}))
}

pub(crate) fn remove(region: &mut Region, input: &Value) -> OpResult {
    let param = resource(region, input)?;
    for key in strings(input, "TagKeys") {
        param.tags.shift_remove(&key);
    }
    Ok(json!({}))
}

pub(crate) fn list(region: &mut Region, input: &Value) -> OpResult {
    let param = resource(region, input)?;
    let tags: Vec<Value> = param
        .tags
        .iter()
        .map(|(k, v)| json!({ "Key": k, "Value": v }))
        .collect();
    Ok(json!({ "TagList": tags }))
}

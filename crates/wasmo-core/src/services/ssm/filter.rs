use serde_json::{Value, json};

use super::params::{latest_json, metadata_fields};
use super::{Parameter, Region, arn, err, page, require, rooted, seconds, validation};
use crate::codec::AwsError;
use crate::service::{Ctx, InputExt, OpResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    Describe,
    ByPath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Filter {
    key: String,
    option: String,
    values: Vec<String>,
}

fn options_for(key: &str) -> &'static [&'static str] {
    match key {
        "Name" => &["Equals", "BeginsWith", "Contains"],
        "Path" => &["Recursive", "OneLevel"],
        "Label" => &["Equals"],
        _ => &["Equals", "BeginsWith"],
    }
}

fn key_allowed(key: &str, scope: Scope) -> bool {
    let tag = key.strip_prefix("tag:").is_some_and(|k| !k.is_empty());
    match scope {
        Scope::Describe => {
            tag || matches!(
                key,
                "Name" | "Type" | "KeyId" | "Path" | "Tier" | "DataType"
            )
        }
        Scope::ByPath => matches!(key, "Type" | "KeyId" | "Label"),
    }
}

fn check_values(key: &str, values: &[String]) -> Result<(), AwsError> {
    let enumerated: &[&str] = match key {
        "Type" => &["String", "StringList", "SecureString"],
        "Tier" => &["Standard", "Advanced", "Intelligent-Tiering"],
        _ => &[],
    };
    for v in values {
        if !enumerated.is_empty() && !enumerated.contains(&v.as_str()) {
            return Err(err(
                "InvalidFilterValue",
                format!(
                    "The following filter value is not valid: {v}. Valid values include: [{}].",
                    enumerated.join(", ")
                ),
            ));
        }
        if key == "Path" && !v.starts_with('/') {
            return Err(err(
                "InvalidFilterValue",
                format!(
                    "The parameter doesn't meet the parameter name requirements. The parameter name must begin with a forward slash \"/\". Filter value: {v}"
                ),
            ));
        }
    }
    Ok(())
}

pub(crate) fn parse(input: &Value, scope: Scope) -> Result<Vec<Filter>, AwsError> {
    let mut filters: Vec<Filter> = Vec::new();
    for raw in input.array_field("ParameterFilters") {
        let key = raw.str_field("Key").unwrap_or_default().to_string();
        if !key_allowed(&key, scope) {
            let valid = match scope {
                Scope::Describe => "[Name, Type, KeyId, Path, Tier, DataType, tag:<key>]",
                Scope::ByPath => "[Type, KeyId, Label]",
            };
            return Err(err(
                "InvalidFilterKey",
                format!(
                    "The following filter key is not valid: {key}. Valid filter keys include: {valid}."
                ),
            ));
        }
        if filters.iter().any(|f| f.key == key) {
            return Err(validation(format!(
                "The following filter is duplicated in the request: {key}. A request can contain only one occurrence of a specific filter."
            )));
        }
        let default_option = if key == "Path" { "OneLevel" } else { "Equals" };
        let option = raw
            .str_field("Option")
            .unwrap_or(default_option)
            .to_string();
        let allowed = options_for(if key.starts_with("tag:") { "tag" } else { &key });
        if !allowed.contains(&option.as_str()) {
            return Err(err(
                "InvalidFilterOption",
                format!(
                    "The following filter option is not valid: {option}. Valid options include: [{}].",
                    allowed.join(", ")
                ),
            ));
        }
        let values: Vec<String> = raw
            .array_field("Values")
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        if values.is_empty() && !key.starts_with("tag:") {
            return Err(err(
                "InvalidFilterValue",
                format!("The following filter values are missing : null for filter key {key}."),
            ));
        }
        if values.len() > 50 {
            return Err(err(
                "InvalidFilterValue",
                format!("The following filter key has too many values: {key}. The maximum is 50."),
            ));
        }
        check_values(&key, &values)?;
        filters.push(Filter {
            key,
            option,
            values,
        });
    }
    Ok(filters)
}

fn compare(option: &str, actual: &str, values: &[String]) -> bool {
    values.iter().any(|v| match option {
        "BeginsWith" => actual.starts_with(v.as_str()),
        "Contains" => actual.contains(v.as_str()),
        _ => actual == v,
    })
}

pub(crate) fn under_path(name: &str, path: &str, recursive: bool) -> bool {
    let full = rooted(name);
    let base = path.trim_end_matches('/');
    let Some(rest) = full.strip_prefix(&format!("{base}/")) else {
        return false;
    };
    !rest.is_empty() && (recursive || !rest.contains('/'))
}

impl Filter {
    pub(crate) fn matches(&self, param: &Parameter) -> bool {
        let v = param.latest();
        let values = &self.values;
        match self.key.as_str() {
            "Name" if self.option == "Contains" => compare(&self.option, &param.name, values),
            "Name" => {
                let rooted_values: Vec<String> = values.iter().map(|v| rooted(v)).collect();
                compare(&self.option, &param.path_name(), &rooted_values)
            }
            "Path" => values
                .iter()
                .any(|p| under_path(&param.name, p, self.option == "Recursive")),
            "Type" => compare(&self.option, &v.kind, values),
            "KeyId" => v
                .key_id
                .as_deref()
                .is_some_and(|k| compare(&self.option, k, values)),
            "Tier" => compare(&self.option, &v.tier, values),
            "DataType" => compare(&self.option, &v.data_type, values),
            "Label" => v.labels.iter().any(|l| values.contains(l)),
            key => {
                let tag = key.trim_start_matches("tag:");
                param
                    .tags
                    .get(tag)
                    .is_some_and(|t| values.is_empty() || compare(&self.option, t, values))
            }
        }
    }
}

fn legacy_filters(input: &Value) -> Result<Vec<Filter>, AwsError> {
    input
        .array_field("Filters")
        .iter()
        .map(|raw| {
            let key = raw.str_field("Key").unwrap_or_default();
            if !matches!(key, "Name" | "Type" | "KeyId") {
                return Err(validation(format!(
                    "1 validation error detected: Value '{key}' at 'filters.1.member.key' failed to satisfy constraint: Member must satisfy enum value set: [Name, Type, KeyId]"
                )));
            }
            let values: Vec<String> = raw
                .array_field("Values")
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            if values.is_empty() {
                return Err(validation(
                    "1 validation error detected: Value at 'filters.1.member.values' failed to satisfy constraint: Member must have length greater than or equal to 1",
                ));
            }
            check_values(key, &values)?;
            Ok(Filter {
                key: key.to_string(),
                option: "Equals".to_string(),
                values,
            })
        })
        .collect()
}

fn with_next(mut out: Value, next: Option<String>) -> Value {
    if let Some(next) = next {
        out["NextToken"] = json!(next);
    }
    out
}

pub(crate) fn by_path(region: &mut Region, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let path = require(input, "Path")?;
    if !path.starts_with('/') {
        return Err(validation(
            "The parameter doesn't meet the parameter name requirements. The parameter name must begin with a forward slash \"/\".",
        ));
    }
    if path.trim_matches('/').split('/').count() > 15 {
        return Err(validation(
            "The parameter path exceeds the maximum hierarchy depth of 15 levels.",
        ));
    }
    let recursive = input
        .get("Recursive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let decrypt = input
        .get("WithDecryption")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let filters = parse(input, Scope::ByPath)?;
    let matched: Vec<&Parameter> = region
        .params
        .values()
        .filter(|p| under_path(&p.name, path, recursive))
        .filter(|p| filters.iter().all(|f| f.matches(p)))
        .collect();
    let (matched, next) = page(matched, input, 10, 10)?;
    let params: Vec<Value> = matched
        .into_iter()
        .map(|p| latest_json(ctx, p, decrypt))
        .collect();
    Ok(with_next(json!({ "Parameters": params }), next))
}

fn metadata(ctx: &Ctx<'_>, param: &Parameter) -> Value {
    let v = param.latest();
    let mut out = json!({
        "Name": param.name,
        "ARN": arn(ctx, &param.name),
        "Type": v.kind,
        "LastModifiedDate": seconds(v.modified_ms),
        "LastModifiedUser": v.user,
        "Version": v.number,
        "Tier": v.tier,
        "Policies": v.policies,
        "DataType": v.data_type,
    });
    metadata_fields(&mut out, v);
    out
}

pub(crate) fn describe(region: &mut Region, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let legacy = legacy_filters(input)?;
    let filters = parse(input, Scope::Describe)?;
    if !legacy.is_empty() && !filters.is_empty() {
        return Err(validation(
            "You can use either Filters or ParameterFilters in a single request.",
        ));
    }
    let shared = input
        .get("Shared")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let matched: Vec<&Parameter> = region
        .params
        .values()
        .filter(|_| !shared)
        .filter(|p| legacy.iter().chain(&filters).all(|f| f.matches(p)))
        .collect();
    let (matched, next) = page(matched, input, 50, 50)?;
    let params: Vec<Value> = matched.into_iter().map(|p| metadata(ctx, p)).collect();
    Ok(with_next(json!({ "Parameters": params }), next))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::services::ssm::Version;

    fn param(name: &str, kind: &str, labels: &[&str], tags: &[(&str, &str)]) -> Parameter {
        Parameter {
            name: name.into(),
            versions: vec![Version {
                number: 1,
                value: "v".into(),
                kind: kind.into(),
                description: None,
                allowed_pattern: None,
                key_id: (kind == "SecureString").then(|| "alias/aws/ssm".into()),
                tier: "Standard".into(),
                data_type: "text".into(),
                policies: Vec::new(),
                labels: labels.iter().map(|l| l.to_string()).collect(),
                modified_ms: 0.0,
                user: String::new(),
            }],
            tags: tags
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    fn filters(scope: Scope, raw: Value) -> Result<Vec<Filter>, String> {
        parse(&json!({ "ParameterFilters": raw }), scope).map_err(|e| e.code)
    }

    #[test]
    fn path_levels() {
        assert!(under_path("/a/b", "/a", false));
        assert!(under_path("/a/b", "/a/", false));
        assert!(!under_path("/a/b/c", "/a", false));
        assert!(under_path("/a/b/c", "/a", true));
        assert!(!under_path("/ab", "/a", true));
        assert!(!under_path("/a", "/a", true));
        assert!(under_path("flat", "/", false));
        assert!(under_path("/x/y", "/", true));
        assert!(!under_path("/x/y", "/", false));
    }

    #[test]
    fn filter_keys_depend_on_scope() {
        assert!(filters(Scope::ByPath, json!([{ "Key": "Label", "Values": ["l"] }])).is_ok());
        assert_eq!(
            filters(Scope::ByPath, json!([{ "Key": "Name", "Values": ["x"] }])),
            Err("InvalidFilterKey".into())
        );
        assert_eq!(
            filters(
                Scope::Describe,
                json!([{ "Key": "Label", "Values": ["l"] }])
            ),
            Err("InvalidFilterKey".into())
        );
        assert_eq!(
            filters(
                Scope::Describe,
                json!([{ "Key": "Type", "Option": "Contains", "Values": ["String"] }])
            ),
            Err("InvalidFilterOption".into())
        );
        assert_eq!(
            filters(
                Scope::Describe,
                json!([{ "Key": "Type", "Values": ["Bogus"] }])
            ),
            Err("InvalidFilterValue".into())
        );
        assert_eq!(
            filters(
                Scope::Describe,
                json!([{ "Key": "Path", "Values": ["nope"] }])
            ),
            Err("InvalidFilterValue".into())
        );
        assert_eq!(
            filters(Scope::Describe, json!([{ "Key": "Name" }])),
            Err("InvalidFilterValue".into())
        );
        assert!(filters(Scope::Describe, json!([{ "Key": "tag:env" }])).is_ok());
    }

    #[test]
    fn filter_matching() {
        let secure = param(
            "/app/db/password",
            "SecureString",
            &["prod"],
            &[("env", "prod")],
        );
        let plain = param("plain", "String", &[], &[]);
        let one = |raw: Value| filters(Scope::Describe, json!([raw])).unwrap().remove(0);
        let name = one(json!({ "Key": "Name", "Option": "BeginsWith", "Values": ["app/"] }));
        assert!(name.matches(&secure) && !name.matches(&plain));
        let contains = one(json!({ "Key": "Name", "Option": "Contains", "Values": ["lai"] }));
        assert!(contains.matches(&plain));
        let path = one(json!({ "Key": "Path", "Option": "Recursive", "Values": ["/app"] }));
        assert!(path.matches(&secure) && !path.matches(&plain));
        let one_level = one(json!({ "Key": "Path", "Values": ["/app"] }));
        assert!(!one_level.matches(&secure));
        let key = one(json!({ "Key": "KeyId", "Values": ["alias/aws/ssm"] }));
        assert!(key.matches(&secure) && !key.matches(&plain));
        let tag = one(json!({ "Key": "tag:env" }));
        assert!(tag.matches(&secure) && !tag.matches(&plain));
        let tag_value = one(json!({ "Key": "tag:env", "Values": ["dev"] }));
        assert!(!tag_value.matches(&secure));
        let label = filters(
            Scope::ByPath,
            json!([{ "Key": "Label", "Values": ["prod"] }]),
        )
        .unwrap();
        assert!(label[0].matches(&secure));
    }
}

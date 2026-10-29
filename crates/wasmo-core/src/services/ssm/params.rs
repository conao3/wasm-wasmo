use serde_json::{Value, json};

use super::pattern::Pattern;
use super::{
    DEFAULT_KEY_ID, Parameter, Region, Version, arn, err, missing, not_found, page, require,
    seconds, shown_value, strings, validation, version_not_found,
};
use crate::codec::AwsError;
use crate::service::{Ctx, InputExt, OpResult};

const MAX_VERSIONS: usize = 100;
const MAX_LEVELS: usize = 15;
const MAX_LABELS: usize = 10;
const MAX_STANDARD_PARAMETERS: usize = 10_000;
const STANDARD_VALUE_LIMIT: usize = 4096;
const ADVANCED_VALUE_LIMIT: usize = 8192;
const TYPES: [&str; 3] = ["String", "StringList", "SecureString"];
const TIERS: [&str; 3] = ["Standard", "Advanced", "Intelligent-Tiering"];
const POLICY_TYPES: [&str; 3] = [
    "Expiration",
    "ExpirationNotification",
    "NoChangeNotification",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Selector {
    Version(u64),
    Label(String),
}

impl Selector {
    fn text(&self) -> String {
        match self {
            Selector::Version(v) => format!(":{v}"),
            Selector::Label(l) => format!(":{l}"),
        }
    }
}

pub(crate) fn split_selector(raw: &str) -> Option<(String, Option<Selector>)> {
    let body_start = if raw.starts_with("arn:") {
        raw.match_indices(':')
            .nth(4)
            .map_or(raw.len(), |(i, _)| i + 1)
    } else {
        0
    };
    let Some((name, selector)) = raw[body_start..]
        .split_once(':')
        .map(|(n, s)| (format!("{}{n}", &raw[..body_start]), s))
    else {
        return Some((raw.to_string(), None));
    };
    if selector.is_empty() || selector.contains(':') {
        return None;
    }
    let selector = if selector.bytes().all(|b| b.is_ascii_digit()) {
        Selector::Version(selector.parse().ok()?)
    } else {
        Selector::Label(selector.to_string())
    };
    Some((name, Some(selector)))
}

pub(crate) fn valid_label(label: &str) -> bool {
    let lower = label.to_ascii_lowercase();
    (1..=100).contains(&label.len())
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && !label.starts_with(|c: char| c.is_ascii_digit())
        && !lower.starts_with("aws")
        && !lower.starts_with("ssm")
}

pub(crate) fn validate_name(name: &str) -> Result<(), AwsError> {
    if name.len() > 1011 {
        return Err(validation(format!(
            "The parameter name {name} exceeds the maximum length of 1011 characters."
        )));
    }
    let lower = name.trim_start_matches('/').to_ascii_lowercase();
    if lower.starts_with("aws") || lower.starts_with("ssm") {
        return Err(validation(
            "Parameter name: can't be prefixed with \"aws\" or \"ssm\" (case-insensitive). If formed as a path, it can consist of sub-paths divided by slash symbol; each sub-path can be formed as a mix of letters, numbers and the following 3 symbols .-_",
        ));
    }
    let chars_ok = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/'));
    if !chars_ok || name.contains("//") || name.ends_with('/') {
        return Err(validation(
            "Parameter name must be a fully qualified name or a path; it can contain only letters, numbers and the symbols .-_/",
        ));
    }
    if name.contains('/') && !name.starts_with('/') {
        return Err(validation(
            "Parameter name must be a fully qualified name. A fully qualified name begins with a forward slash \"/\".",
        ));
    }
    if name.trim_start_matches('/').split('/').count() > MAX_LEVELS {
        return Err(err(
            "HierarchyLevelLimitExceededException",
            "A hierarchy can have a maximum of 15 levels. For more information, see https://docs.aws.amazon.com/systems-manager/latest/userguide/sysman-paramstore-working.html",
        ));
    }
    Ok(())
}

fn lookup<'a>(
    region: &'a Region,
    raw: &str,
) -> Result<(&'a Parameter, &'a Version, Option<Selector>), AwsError> {
    let (name, selector) = split_selector(raw).ok_or_else(|| not_found(raw))?;
    let name = region.resolve_name(&name);
    let param = region.params.get(&name).ok_or_else(|| not_found(raw))?;
    let version = match &selector {
        None => param.latest(),
        Some(Selector::Version(n)) => param
            .version(*n)
            .ok_or_else(|| version_not_found(&name, *n))?,
        Some(Selector::Label(l)) => param.labelled(l).ok_or_else(|| not_found(raw))?,
    };
    Ok((param, version, selector))
}

fn parameter_json(
    ctx: &Ctx<'_>,
    param: &Parameter,
    v: &Version,
    selector: Option<&Selector>,
    decrypt: bool,
) -> Value {
    let mut out = json!({
        "Name": param.name,
        "Type": v.kind,
        "Value": shown_value(v, decrypt),
        "Version": v.number,
        "LastModifiedDate": seconds(v.modified_ms),
        "ARN": arn(ctx, &param.name),
        "DataType": v.data_type,
    });
    if let Some(s) = selector {
        out["Selector"] = json!(s.text());
    }
    out
}

pub(crate) fn latest_json(ctx: &Ctx<'_>, param: &Parameter, decrypt: bool) -> Value {
    parameter_json(ctx, param, param.latest(), None, decrypt)
}

fn decrypt(input: &Value) -> bool {
    input
        .get("WithDecryption")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub(crate) fn get(region: &mut Region, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let raw = require(input, "Name")?;
    let (param, v, selector) = lookup(region, raw)?;
    Ok(json!({ "Parameter": parameter_json(ctx, param, v, selector.as_ref(), decrypt(input)) }))
}

pub(crate) fn get_many(region: &mut Region, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let mut names = strings(input, "Names");
    if names.is_empty() || names.len() > 10 {
        return Err(validation(format!(
            "1 validation error detected: Value '{names:?}' at 'names' failed to satisfy constraint: Member must have length less than or equal to 10"
        )));
    }
    let mut seen = Vec::new();
    names.retain(|n| {
        let fresh = !seen.contains(n);
        seen.push(n.clone());
        fresh
    });
    let (mut found, mut invalid) = (Vec::new(), Vec::new());
    for raw in names {
        match lookup(region, &raw) {
            Ok((param, v, selector)) => found.push(parameter_json(
                ctx,
                param,
                v,
                selector.as_ref(),
                decrypt(input),
            )),
            Err(_) => invalid.push(raw),
        }
    }
    Ok(json!({ "Parameters": found, "InvalidParameters": invalid }))
}

fn tag_list(input: &Value) -> Result<indexmap::IndexMap<String, String>, AwsError> {
    input
        .array_field("Tags")
        .iter()
        .map(|t| {
            let key = t
                .str_field("Key")
                .ok_or_else(|| validation("Tag Key is required."))?;
            let value = t.get("Value").and_then(Value::as_str).unwrap_or_default();
            Ok((key.to_string(), value.to_string()))
        })
        .collect()
}

fn policies(raw: Option<&str>) -> Result<Vec<Value>, AwsError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let invalid = || {
        err(
            "InvalidPolicyTypeException",
            "A policy type is not supported. Parameter Store supports the following policy types: Expiration, ExpirationNotification, and NoChangeNotification.",
        )
    };
    let parsed: Value = serde_json::from_str(raw).map_err(|_| invalid())?;
    let list = match parsed {
        Value::Array(list) => list,
        single @ Value::Object(_) => vec![single],
        _ => return Err(invalid()),
    };
    if list.len() > 10 {
        return Err(err(
            "PoliciesLimitExceededException",
            "You specified more than the maximum number of allowed policies for the parameter. The maximum is 10.",
        ));
    }
    list.into_iter()
        .map(|p| {
            let kind = p.str_field("Type").ok_or_else(invalid)?;
            if !POLICY_TYPES.contains(&kind) {
                return Err(invalid());
            }
            if !p.get("Attributes").is_some_and(Value::is_object) {
                return Err(err(
                    "InvalidPolicyAttributeException",
                    format!("A policy attribute or its value is invalid for the {kind} policy."),
                ));
            }
            Ok(json!({ "PolicyText": p.to_string(), "PolicyType": kind, "PolicyStatus": "Pending" }))
        })
        .collect()
}

fn valid_key_id(key: &str) -> bool {
    let uuid = key.len() == 36
        && key.chars().enumerate().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        });
    uuid || key.starts_with("alias/") && key.len() > 6
        || key.starts_with("mrk-")
        || key.starts_with("arn:aws:kms:")
}

fn tier(
    requested: Option<&str>,
    existing: Option<&Version>,
    value: &str,
    has_policies: bool,
) -> Result<String, AwsError> {
    if let Some(t) = requested.filter(|t| !TIERS.contains(t)) {
        return Err(validation(format!(
            "1 validation error detected: Value '{t}' at 'tier' failed to satisfy constraint: Member must satisfy enum value set: [Standard, Advanced, Intelligent-Tiering]"
        )));
    }
    let was_advanced = existing.is_some_and(|v| v.tier == "Advanced");
    let needs_advanced = value.len() > STANDARD_VALUE_LIMIT || has_policies;
    let tier = match requested {
        Some("Intelligent-Tiering") if was_advanced || needs_advanced => "Advanced",
        Some("Intelligent-Tiering") => "Standard",
        Some(t) => t,
        None if was_advanced => "Advanced",
        None => "Standard",
    };
    if tier == "Standard" && was_advanced {
        return Err(validation(
            "This parameter uses the advanced-parameter tier. You can't downgrade a parameter from the advanced-parameter tier to the standard-parameter tier. If necessary, you can delete the advanced parameter and recreate it as a standard parameter.",
        ));
    }
    if tier == "Standard" && value.len() > STANDARD_VALUE_LIMIT {
        return Err(validation(
            "Standard tier parameters support a maximum parameter value of 4096 characters. To create a larger parameter value, upgrade the parameter to use the advanced-parameter tier. For more information, see https://docs.aws.amazon.com/systems-manager/latest/userguide/parameter-store-advanced-parameters.html",
        ));
    }
    if tier == "Standard" && has_policies {
        return Err(validation(
            "Standard tier parameters don't support parameter policies. To attach a policy to a parameter, upgrade the parameter to use the advanced-parameter tier.",
        ));
    }
    if value.len() > ADVANCED_VALUE_LIMIT {
        return Err(validation(
            "Advanced tier parameters support a maximum parameter value of 8192 characters.",
        ));
    }
    Ok(tier.to_string())
}

fn check_data_type(data_type: &str, kind: &str, value: &str) -> Result<(), AwsError> {
    match data_type {
        "text" => Ok(()),
        "aws:ec2:image" | "aws:ssm:integration" if kind != "String" => Err(validation(format!(
            "The data type {data_type} is supported only for String parameters."
        ))),
        "aws:ec2:image" => {
            let id = value.strip_prefix("ami-").unwrap_or_default();
            let valid = (8..=17).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit());
            if valid || value.starts_with("resolve:ssm:") {
                Ok(())
            } else {
                Err(validation(format!(
                    "Parameter value, {value}, is not a valid AMI ID. AMI IDs must be in the format ami-12345abc or ami-12345678901abcdef."
                )))
            }
        }
        "aws:ssm:integration" => Ok(()),
        other => Err(validation(format!(
            "The following data type is not supported: {other} (Data type names are all lowercase.)"
        ))),
    }
}

fn check_pattern(pattern: Option<&str>, value: &str) -> Result<(), AwsError> {
    let Some(pattern) = pattern else {
        return Ok(());
    };
    if pattern.len() > 1024 {
        return Err(err(
            "InvalidAllowedPatternException",
            "The allowed pattern exceeds the maximum length of 1024 characters.",
        ));
    }
    let compiled = Pattern::compile(pattern).map_err(|e| {
        err(
            "InvalidAllowedPatternException",
            format!("The allowed pattern is not a valid regular expression: {e}"),
        )
    })?;
    if compiled.full_match(value) {
        Ok(())
    } else {
        Err(err(
            "ParameterPatternMismatchException",
            format!("Parameter value, cannot be validated against allowedPattern: {pattern}"),
        ))
    }
}

pub(crate) fn put(region: &mut Region, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let name = require(input, "Name")?.to_string();
    validate_name(&name)?;
    let value = require(input, "Value")?.to_string();
    let overwrite = input
        .get("Overwrite")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let existing = region.params.get(&name).map(Parameter::latest);
    if existing.is_some() && !overwrite {
        return Err(err(
            "ParameterAlreadyExists",
            "The parameter already exists. To overwrite this value, set the overwrite option in the request to true.",
        ));
    }
    let tags = tag_list(input)?;
    if overwrite && !tags.is_empty() {
        return Err(validation(
            "Invalid request: tags and overwrite can't be used together. To create a parameter with tags, please remove overwrite flag. To update tags for an existing parameter, please use AddTagsToResource or RemoveTagsFromResource.",
        ));
    }
    let kind = match (input.str_field("Type"), existing) {
        (Some(t), _) if TYPES.contains(&t) => t.to_string(),
        (Some(t), _) => {
            return Err(validation(format!(
                "1 validation error detected: Value '{t}' at 'type' failed to satisfy constraint: Member must satisfy enum value set: [SecureString, StringList, String]"
            )));
        }
        (None, Some(v)) => v.kind.clone(),
        (None, None) => {
            return Err(validation(
                "A parameter type is required when you create a parameter.",
            ));
        }
    };
    let description = input
        .str_field("Description")
        .map(str::to_string)
        .or_else(|| existing.and_then(|v| v.description.clone()));
    if description.as_ref().is_some_and(|d| d.len() > 1024) {
        return Err(validation(
            "1 validation error detected: Value at 'description' failed to satisfy constraint: Member must have length less than or equal to 1024",
        ));
    }
    let data_type = input
        .str_field("DataType")
        .map(str::to_string)
        .or_else(|| existing.map(|v| v.data_type.clone()))
        .unwrap_or_else(|| "text".to_string());
    check_data_type(&data_type, &kind, &value)?;
    let key_id = if kind == "SecureString" {
        let key = input
            .str_field("KeyId")
            .map(str::to_string)
            .or_else(|| existing.and_then(|v| v.key_id.clone()))
            .unwrap_or_else(|| DEFAULT_KEY_ID.to_string());
        if !valid_key_id(&key) {
            return Err(err("InvalidKeyId", format!("Invalid KeyId: {key}")));
        }
        Some(key)
    } else {
        None
    };
    let allowed_pattern = input
        .str_field("AllowedPattern")
        .map(str::to_string)
        .or_else(|| existing.and_then(|v| v.allowed_pattern.clone()));
    check_pattern(allowed_pattern.as_deref(), &value)?;
    let policies = policies(input.str_field("Policies"))?;
    let tier = tier(
        input.str_field("Tier"),
        existing,
        &value,
        !policies.is_empty(),
    )?;
    if existing.is_none()
        && tier == "Standard"
        && region
            .params
            .values()
            .filter(|p| p.latest().tier == "Standard")
            .count()
            >= MAX_STANDARD_PARAMETERS
    {
        return Err(err(
            "ParameterLimitExceeded",
            "You have exceeded the number of parameters for this AWS account. Delete one or more parameters and try again.",
        ));
    }
    let number = existing.map_or(1, |v| v.number + 1);
    let version = Version {
        number,
        value,
        kind,
        description,
        allowed_pattern,
        key_id,
        tier: tier.clone(),
        data_type,
        policies,
        labels: Vec::new(),
        modified_ms: ctx.now_ms(),
        user: format!("arn:aws:iam::{}:root", ctx.account),
    };
    let param = region
        .params
        .entry(name.clone())
        .or_insert_with(|| Parameter {
            name: name.clone(),
            versions: Vec::new(),
            tags,
        });
    if param.versions.len() >= MAX_VERSIONS {
        let oldest = &param.versions[0];
        if !oldest.labels.is_empty() {
            return Err(err(
                "ParameterMaxVersionLimitExceeded",
                format!(
                    "You attempted to create a new version of {name} by calling the PutParameter API with the overwrite flag. Version {}, the oldest version, can't be deleted because it has a label associated with it. Move the label to another version of the parameter, and try again.",
                    oldest.number
                ),
            ));
        }
        param.versions.remove(0);
    }
    param.versions.push(version);
    Ok(json!({ "Version": number, "Tier": tier }))
}

pub(crate) fn delete(region: &mut Region, input: &Value) -> OpResult {
    let raw = require(input, "Name")?;
    let name = region.resolve_name(raw);
    region
        .params
        .remove(&name)
        .map(|_| json!({}))
        .ok_or_else(|| not_found(raw))
}

pub(crate) fn delete_many(region: &mut Region, input: &Value) -> OpResult {
    let names = strings(input, "Names");
    if names.is_empty() || names.len() > 10 {
        return Err(validation(
            "1 validation error detected: Value at 'names' failed to satisfy constraint: Member must have length less than or equal to 10",
        ));
    }
    let (mut deleted, mut invalid) = (Vec::new(), Vec::new());
    for raw in names {
        let name = region.resolve_name(&raw);
        if region.params.remove(&name).is_some() {
            deleted.push(raw);
        } else if !invalid.contains(&raw) && !deleted.contains(&raw) {
            invalid.push(raw);
        }
    }
    Ok(json!({ "DeletedParameters": deleted, "InvalidParameters": invalid }))
}

pub(crate) fn history(region: &mut Region, input: &Value) -> OpResult {
    let raw = require(input, "Name")?;
    let param = region
        .params
        .get(&region.resolve_name(raw))
        .ok_or_else(|| not_found(raw))?;
    let decrypt = decrypt(input);
    let entries: Vec<Value> = param
        .versions
        .iter()
        .map(|v| {
            let mut out = json!({
                "Name": param.name,
                "Type": v.kind,
                "Value": shown_value(v, decrypt),
                "Version": v.number,
                "Labels": v.labels,
                "LastModifiedDate": seconds(v.modified_ms),
                "LastModifiedUser": v.user,
                "Tier": v.tier,
                "Policies": v.policies,
                "DataType": v.data_type,
            });
            metadata_fields(&mut out, v);
            out
        })
        .collect();
    let (entries, next) = page(entries, input, 50, 50)?;
    let mut out = json!({ "Parameters": entries });
    if let Some(next) = next {
        out["NextToken"] = json!(next);
    }
    Ok(out)
}

pub(crate) fn metadata_fields(out: &mut Value, v: &Version) {
    for (key, field) in [
        ("Description", &v.description),
        ("AllowedPattern", &v.allowed_pattern),
        ("KeyId", &v.key_id),
    ] {
        if let Some(field) = field {
            out[key] = json!(field);
        }
    }
}

fn target<'a>(
    region: &'a mut Region,
    input: &Value,
) -> Result<(&'a mut Parameter, usize), AwsError> {
    let raw = require(input, "Name")?;
    let name = region.resolve_name(raw);
    let param = region.params.get_mut(&name).ok_or_else(|| not_found(raw))?;
    let number = input
        .i64_field("ParameterVersion")
        .map_or_else(|| param.latest().number, |n| n.max(0) as u64);
    let index = param
        .versions
        .iter()
        .position(|v| v.number == number)
        .ok_or_else(|| version_not_found(&name, number))?;
    Ok((param, index))
}

fn labels_input(input: &Value) -> Result<Vec<String>, AwsError> {
    let labels = strings(input, "Labels");
    if labels.is_empty() || labels.len() > MAX_LABELS {
        return Err(validation(
            "1 validation error detected: Value at 'labels' failed to satisfy constraint: Member must have length less than or equal to 10 and greater than or equal to 1",
        ));
    }
    Ok(labels)
}

pub(crate) fn label(region: &mut Region, input: &Value) -> OpResult {
    let labels = labels_input(input)?;
    let (param, index) = target(region, input)?;
    let (valid, invalid): (Vec<String>, Vec<String>) =
        labels.into_iter().partition(|l| valid_label(l));
    let mut merged = param.versions[index].labels.clone();
    for l in &valid {
        if !merged.contains(l) {
            merged.push(l.clone());
        }
    }
    if merged.len() > MAX_LABELS {
        return Err(err(
            "ParameterVersionLabelLimitExceeded",
            "A parameter version can have maximum 10 labels.Move one or more labels to another version and try again.",
        ));
    }
    for v in &mut param.versions {
        v.labels.retain(|l| !valid.contains(l));
    }
    param.versions[index].labels = merged;
    Ok(json!({ "InvalidLabels": invalid, "ParameterVersion": param.versions[index].number }))
}

pub(crate) fn unlabel(region: &mut Region, input: &Value) -> OpResult {
    if input.get("ParameterVersion").is_none_or(Value::is_null) {
        return Err(missing("ParameterVersion"));
    }
    let labels = labels_input(input)?;
    let (param, index) = target(region, input)?;
    let version = &mut param.versions[index];
    let (removed, invalid): (Vec<String>, Vec<String>) =
        labels.into_iter().partition(|l| version.labels.contains(l));
    version.labels.retain(|l| !removed.contains(l));
    Ok(json!({ "RemovedLabels": removed, "InvalidLabels": invalid }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors() {
        assert_eq!(split_selector("/a/b"), Some(("/a/b".into(), None)));
        assert_eq!(
            split_selector("/a/b:3"),
            Some(("/a/b".into(), Some(Selector::Version(3))))
        );
        assert_eq!(
            split_selector("name:prod"),
            Some(("name".into(), Some(Selector::Label("prod".into()))))
        );
        assert_eq!(
            split_selector("arn:aws:ssm:us-east-1:000000000000:parameter/a/b:7"),
            Some((
                "arn:aws:ssm:us-east-1:000000000000:parameter/a/b".into(),
                Some(Selector::Version(7))
            ))
        );
        assert_eq!(split_selector("a:1:2"), None);
        assert_eq!(split_selector("a:"), None);
    }

    #[test]
    fn labels() {
        for ok in ["prod", "Release-1.2_x", "a"] {
            assert!(valid_label(ok), "{ok}");
        }
        for bad in [
            "1abc",
            "awsLabel",
            "SSMx",
            "with/slash",
            "has space",
            "",
            &"x".repeat(101),
        ] {
            assert!(!valid_label(bad), "{bad}");
        }
    }

    #[test]
    fn names() {
        assert!(validate_name("plain").is_ok());
        assert!(validate_name("/a/b.c-d_e").is_ok());
        assert_eq!(
            validate_name("a/b").unwrap_err().code,
            "ValidationException"
        );
        assert_eq!(
            validate_name("/aws/x").unwrap_err().code,
            "ValidationException"
        );
        assert_eq!(
            validate_name("/a//b").unwrap_err().code,
            "ValidationException"
        );
        assert_eq!(
            validate_name("/a b").unwrap_err().code,
            "ValidationException"
        );
        let deep = format!("/{}", vec!["l"; 15].join("/"));
        assert!(validate_name(&deep).is_ok());
        assert_eq!(
            validate_name(&format!("{deep}/x")).unwrap_err().code,
            "HierarchyLevelLimitExceededException"
        );
    }

    #[test]
    fn arn_names_resolve_to_stored_names() {
        let mut region = Region::default();
        for name in ["flat", "/a/b"] {
            region.params.insert(
                name.into(),
                Parameter {
                    name: name.into(),
                    versions: Vec::new(),
                    tags: indexmap::IndexMap::new(),
                },
            );
        }
        let arn = |r: &str| format!("arn:aws:ssm:us-east-1:000000000000:parameter/{r}");
        assert_eq!(region.resolve_name(&arn("flat")), "flat");
        assert_eq!(region.resolve_name(&arn("a/b")), "/a/b");
        assert_eq!(region.resolve_name(&arn("other")), "/other");
        assert_eq!(region.resolve_name("flat"), "flat");
    }
}

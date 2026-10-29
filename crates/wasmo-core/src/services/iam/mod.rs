mod account;
mod policies;
mod policy;
mod principals;

use std::collections::{BTreeMap, HashMap};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use crate::codec::AwsError;
use crate::codec::rest::uri_encode;
use crate::embedded_model;
use crate::service::{Ctx, InputExt, OpResult, Service, ServiceModel};

pub type Tags = Vec<(String, String)>;
pub type Handled = Result<Option<Value>, AwsError>;

#[derive(Debug, Clone, Default)]
pub struct User {
    pub name: String,
    pub id: String,
    pub path: String,
    pub created: f64,
    pub tags: Tags,
    pub inline: BTreeMap<String, String>,
    pub attached: Vec<String>,
    pub boundary: Option<String>,
    pub login: Option<(f64, bool)>,
    pub mfa: Vec<(String, f64)>,
}

#[derive(Debug, Clone, Default)]
pub struct Group {
    pub name: String,
    pub id: String,
    pub path: String,
    pub created: f64,
    pub members: Vec<String>,
    pub inline: BTreeMap<String, String>,
    pub attached: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Role {
    pub name: String,
    pub id: String,
    pub path: String,
    pub created: f64,
    pub trust: String,
    pub description: Option<String>,
    pub max_session: i64,
    pub tags: Tags,
    pub inline: BTreeMap<String, String>,
    pub attached: Vec<String>,
    pub boundary: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ManagedPolicy {
    pub name: String,
    pub id: String,
    pub path: String,
    pub arn: String,
    pub description: Option<String>,
    pub created: f64,
    pub updated: f64,
    pub default_version: String,
    pub versions: Vec<(String, String, f64)>,
    pub next_version: u32,
    pub tags: Tags,
}

#[derive(Debug, Clone, Default)]
pub struct InstanceProfile {
    pub name: String,
    pub id: String,
    pub path: String,
    pub created: f64,
    pub roles: Vec<String>,
    pub tags: Tags,
}

#[derive(Debug, Clone, Default)]
pub struct AccessKey {
    pub user: String,
    pub id: String,
    pub secret: String,
    pub status: String,
    pub created: f64,
}

#[derive(Debug, Default)]
pub struct Account {
    pub users: BTreeMap<String, User>,
    pub groups: BTreeMap<String, Group>,
    pub roles: BTreeMap<String, Role>,
    pub policies: BTreeMap<String, ManagedPolicy>,
    pub profiles: BTreeMap<String, InstanceProfile>,
    pub access_keys: Vec<AccessKey>,
    pub oidc: BTreeMap<String, Value>,
    pub saml: BTreeMap<String, Value>,
    pub aliases: Vec<String>,
    pub password_policy: Option<Value>,
    pub server_certs: BTreeMap<String, Value>,
    pub ssh_keys: Vec<Value>,
    pub signing_certs: Vec<Value>,
    pub virtual_mfa: BTreeMap<String, Value>,
    pub service_creds: Vec<Value>,
    pub credential_report: Option<f64>,
    pub deletion_tasks: BTreeMap<String, String>,
    pub access_jobs: BTreeMap<String, f64>,
    pub resource_tags: BTreeMap<String, Tags>,
    pub sts_token_version: Option<String>,
}

#[derive(Default)]
pub struct Iam {
    accounts: HashMap<String, Account>,
}

pub fn no_such(message: impl Into<String>) -> AwsError {
    AwsError::new(404, "NoSuchEntity", message)
}

pub fn exists(message: impl Into<String>) -> AwsError {
    AwsError::new(409, "EntityAlreadyExists", message)
}

pub fn conflict(message: impl Into<String>) -> AwsError {
    AwsError::new(409, "DeleteConflict", message)
}

pub fn invalid(message: impl Into<String>) -> AwsError {
    AwsError::new(400, "InvalidInput", message)
}

pub fn required<'a>(input: &'a Value, key: &str) -> Result<&'a str, AwsError> {
    input.str_field(key).ok_or_else(|| {
        AwsError::new(
            400,
            "ValidationError",
            format!(
                "1 validation error detected: Value null at '{}' failed to satisfy constraint: Member must not be null",
                lower_first(key)
            ),
        )
    })
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_lowercase().chain(c).collect())
        .unwrap_or_default()
}

pub fn strings(v: &[Value]) -> Vec<String> {
    v.iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

pub fn now(ctx: &Ctx<'_>) -> f64 {
    (ctx.now_ms() / 1000.0).floor()
}

/// An IAM unique ID: a four-letter prefix and 17 base32 characters.
pub fn unique_id(ctx: &mut Ctx<'_>, prefix: &str) -> String {
    format!(
        "{prefix}{}",
        random_chars(ctx, 17, b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567")
    )
}

pub fn random_chars(ctx: &mut Ctx<'_>, n: usize, alphabet: &[u8]) -> String {
    let mut b = vec![0u8; n];
    ctx.random_bytes(&mut b);
    b.iter()
        .map(|x| alphabet[*x as usize % alphabet.len()] as char)
        .collect()
}

pub fn path_of(input: &Value) -> Result<String, AwsError> {
    let p = input.str_field("Path").unwrap_or("/");
    if !p.starts_with('/') || !p.ends_with('/') {
        return Err(AwsError::new(
            400,
            "ValidationError",
            "The specified value for path is invalid. It must begin and end with / and contain only alphanumeric characters and/or / characters.",
        ));
    }
    Ok(p.to_string())
}

pub fn arn(ctx: &Ctx<'_>, kind: &str, path: &str, name: &str) -> String {
    format!("arn:aws:iam::{}:{kind}{path}{name}", ctx.account)
}

/// Policy documents travel URL-encoded in IAM responses.
pub fn encode_doc(doc: &str) -> String {
    uri_encode(doc, false)
}

pub fn validated(input: &Value, key: &str) -> Result<String, AwsError> {
    let doc = required(input, key)?.to_string();
    policy::parse(&doc).map_err(|m| AwsError::new(400, "MalformedPolicyDocument", m))?;
    Ok(doc)
}

pub fn tags_of(input: &Value) -> Result<Tags, AwsError> {
    let tags: Tags = input
        .array_field("Tags")
        .iter()
        .filter_map(|t| {
            Some((
                t.str_field("Key")?.to_string(),
                t.get("Value")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ))
        })
        .collect();
    if tags.len() > 50 {
        return Err(AwsError::new(
            409,
            "LimitExceeded",
            "1 validation error detected: Value at 'tags' failed to satisfy constraint: Member must have length less than or equal to 50.",
        ));
    }
    Ok(tags)
}

pub fn merge_tags(into: &mut Tags, tags: Tags) {
    for (k, v) in tags {
        match into.iter_mut().find(|(x, _)| *x == k) {
            Some(t) => t.1 = v,
            None => into.push((k, v)),
        }
    }
}

pub fn tag_values(tags: &Tags) -> Value {
    Value::Array(
        tags.iter()
            .map(|(k, v)| json!({ "Key": k, "Value": v }))
            .collect(),
    )
}

/// Applies Tag* / Untag* / List*Tags to `tags`.
pub fn tag_op(tags: &mut Tags, verb: &str, input: &Value) -> Result<Value, AwsError> {
    Ok(match verb {
        "Tag" => {
            let new = tags_of(input)?;
            merge_tags(tags, new);
            if tags.len() > 50 {
                return Err(AwsError::new(
                    409,
                    "LimitExceeded",
                    "The number of tags has reached the maximum limit.",
                ));
            }
            json!({})
        }
        "Untag" => {
            let keys = strings(input.array_field("TagKeys"));
            tags.retain(|(k, _)| !keys.contains(k));
            json!({})
        }
        _ => page_value(
            tag_values(tags).as_array().cloned().unwrap_or_default(),
            input,
            "Tags",
        ),
    })
}

/// IAM-style pagination: `Marker` / `MaxItems` in, `IsTruncated` / `Marker` out.
pub fn page_value(items: Vec<Value>, input: &Value, key: &str) -> Value {
    let start = input
        .str_field("Marker")
        .and_then(|t| STANDARD.decode(t).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0)
        .min(items.len());
    let max = input
        .i64_field("MaxItems")
        .map_or(100, |m| m.max(1) as usize);
    let end = (start + max).min(items.len());
    let mut out = json!({ key: items[start..end].to_vec(), "IsTruncated": end < items.len() });
    if end < items.len() {
        out["Marker"] = json!(STANDARD.encode(end.to_string()));
    }
    out
}

pub fn name_from_arn(arn: &str) -> &str {
    arn.rsplit('/').next().unwrap_or(arn)
}

impl Account {
    pub fn user(&self, name: &str) -> Result<&User, AwsError> {
        self.users
            .get(name)
            .ok_or_else(|| no_such(format!("The user with name {name} cannot be found.")))
    }

    pub fn user_mut(&mut self, name: &str) -> Result<&mut User, AwsError> {
        self.users
            .get_mut(name)
            .ok_or_else(|| no_such(format!("The user with name {name} cannot be found.")))
    }

    pub fn group_mut(&mut self, name: &str) -> Result<&mut Group, AwsError> {
        self.groups
            .get_mut(name)
            .ok_or_else(|| no_such(format!("The group with name {name} cannot be found.")))
    }

    pub fn role(&self, name: &str) -> Result<&Role, AwsError> {
        self.roles
            .get(name)
            .ok_or_else(|| no_such(format!("The role with name {name} cannot be found.")))
    }

    pub fn role_mut(&mut self, name: &str) -> Result<&mut Role, AwsError> {
        self.roles
            .get_mut(name)
            .ok_or_else(|| no_such(format!("The role with name {name} cannot be found.")))
    }

    /// The managed policy behind `arn`: one of the account's or an AWS managed policy.
    pub fn policy(&self, arn: &str) -> Result<ManagedPolicy, AwsError> {
        self.policies
            .get(arn)
            .cloned()
            .or_else(|| policies::aws_managed(arn))
            .ok_or_else(|| no_such(format!("Policy {arn} does not exist or is not attachable.")))
    }

    pub fn attachment_count(&self, arn: &str) -> usize {
        let a = |l: &Vec<String>| l.iter().filter(|x| *x == arn).count();
        self.users.values().map(|u| a(&u.attached)).sum::<usize>()
            + self.groups.values().map(|g| a(&g.attached)).sum::<usize>()
            + self.roles.values().map(|r| a(&r.attached)).sum::<usize>()
    }

    pub fn boundary_count(&self, arn: &str) -> usize {
        self.users
            .values()
            .filter(|u| u.boundary.as_deref() == Some(arn))
            .count()
            + self
                .roles
                .values()
                .filter(|r| r.boundary.as_deref() == Some(arn))
                .count()
    }
}

impl Service for Iam {
    fn signing_name(&self) -> &'static str {
        "iam"
    }

    fn model(&self) -> &'static ServiceModel {
        embedded_model!("iam")
    }

    fn call(&mut self, ctx: &mut Ctx<'_>, operation: &str, input: Value) -> OpResult {
        let account = self.accounts.entry(ctx.account.clone()).or_default();
        for part in [
            principals::users,
            principals::roles,
            policies::call,
            account::credentials,
            account::call,
        ] {
            if let Some(out) = part(account, ctx, operation, &input)? {
                return Ok(out);
            }
        }
        Err(AwsError::new(
            400,
            "InvalidAction",
            format!("wasmo: {operation} is not implemented"),
        ))
    }
}

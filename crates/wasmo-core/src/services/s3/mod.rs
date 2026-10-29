use std::collections::{BTreeMap, HashMap};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::codec::AwsError;
use crate::codec::rest::{parse_query, uri_encode};
use crate::embedded_model;
use crate::model::Operation;
use crate::service::{Arn, Ctx, Delivery, InputExt, OpResult, Service, ServiceModel};
use crate::wire::{Request, Response};
use crate::xml::escape;

mod bucket;
mod checksum;
mod list;
mod listing;
mod multipart;
mod notify;
mod object;
mod state;

use state::Bucket;

pub type Buckets = BTreeMap<String, Bucket>;

const XMLNS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";
const KEYED_CONFIGS: &[&str] = &["Analytics", "Inventory", "Metrics", "IntelligentTiering"];

#[derive(Default)]
pub struct S3 {
    accounts: HashMap<String, Buckets>,
}

pub fn err(status: u16, code: &str, message: impl Into<String>) -> AwsError {
    AwsError::new(status, code, message)
}

pub fn with(mut e: AwsError, key: &str, value: impl Into<Value>) -> AwsError {
    e.data.insert(key.to_string(), value.into());
    e
}

pub fn no_such_bucket(name: &str) -> AwsError {
    with(
        err(404, "NoSuchBucket", "The specified bucket does not exist"),
        "BucketName",
        name,
    )
}

pub fn no_such_key(key: &str) -> AwsError {
    with(
        err(404, "NoSuchKey", "The specified key does not exist."),
        "Key",
        key,
    )
}

pub fn no_such_upload(id: &str) -> AwsError {
    with(
        err(
            404,
            "NoSuchUpload",
            "The specified upload does not exist. The upload ID may be invalid, or the upload may have been aborted or completed.",
        ),
        "UploadId",
        id,
    )
}

pub fn access_denied() -> AwsError {
    err(403, "AccessDenied", "Access Denied")
}

pub fn invalid_argument(message: impl Into<String>, name: &str, value: &str) -> AwsError {
    with(
        with(err(400, "InvalidArgument", message), "ArgumentName", name),
        "ArgumentValue",
        value,
    )
}

pub fn now_secs(ctx: &Ctx<'_>) -> f64 {
    (ctx.now_ms() / 1000.0).floor()
}

pub fn version_id(ctx: &mut Ctx<'_>) -> String {
    ctx.uuid().replace('-', "")
}

pub fn owner(ctx: &Ctx<'_>) -> Value {
    let id = checksum::hex(&Sha256::digest(ctx.account.as_bytes()));
    json!({ "ID": id, "DisplayName": "wasmo" })
}

pub fn bucket_mut<'a>(buckets: &'a mut Buckets, input: &Value) -> Result<&'a mut Bucket, AwsError> {
    let name = input.require_str("Bucket")?;
    buckets.get_mut(name).ok_or_else(|| no_such_bucket(name))
}

pub fn bucket_ref<'a>(buckets: &'a Buckets, input: &Value) -> Result<&'a Bucket, AwsError> {
    let name = input.require_str("Bucket")?;
    buckets.get(name).ok_or_else(|| no_such_bucket(name))
}

/// Applies `encoding-type=url` to a listed key or prefix.
pub fn encode_key(s: &str, url: bool) -> String {
    if url {
        uri_encode(s, true)
    } else {
        s.to_string()
    }
}

pub fn tag_set(tags: &[(String, String)]) -> Value {
    Value::Array(
        tags.iter()
            .map(|(k, v)| json!({ "Key": k, "Value": v }))
            .collect(),
    )
}

pub fn tags_from(tagging: Option<&Map<String, Value>>) -> Vec<(String, String)> {
    tagging
        .and_then(|t| t.get("TagSet"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| {
            Some((
                t.get("Key")?.as_str()?.to_string(),
                t.get("Value")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ))
        })
        .collect()
}

pub fn put(out: &mut Map<String, Value>, key: &str, value: Option<impl Into<Value>>) {
    if let Some(v) = value {
        out.insert(key.to_string(), v.into());
    }
}

impl Service for S3 {
    fn signing_name(&self) -> &'static str {
        "s3"
    }

    fn model(&self) -> &'static ServiceModel {
        embedded_model!("s3")
    }

    fn virtual_hosted(&self) -> bool {
        true
    }

    fn resolve(&self, req: &Request) -> Option<&'static Operation> {
        let model = self.model();
        let op = model.resolve_rest(req)?;
        let query = parse_query(&req.query);
        let has = |k: &str| query.iter().any(|(q, _)| q == k);
        let copy = req.header("x-amz-copy-source").is_some();
        let upload = has("uploadId");
        let name = match op.name.as_str() {
            "CopyObject" | "PutObject" | "UploadPart" | "UploadPartCopy" => {
                match (upload && has("partNumber"), copy) {
                    (true, true) => "UploadPartCopy",
                    (true, false) => "UploadPart",
                    (false, true) => "CopyObject",
                    (false, false) => "PutObject",
                }
                .to_string()
            }
            "GetObject" | "ListParts" if upload => "ListParts".to_string(),
            "GetObject" | "ListParts" => "GetObject".to_string(),
            "DeleteObject" | "AbortMultipartUpload" if upload => "AbortMultipartUpload".to_string(),
            "DeleteObject" | "AbortMultipartUpload" => "DeleteObject".to_string(),
            "ListBuckets" | "ListDirectoryBuckets" if has("max-directory-buckets") => {
                "ListDirectoryBuckets".to_string()
            }
            "ListBuckets" | "ListDirectoryBuckets" => "ListBuckets".to_string(),
            "GetBucketLifecycle" => "GetBucketLifecycleConfiguration".to_string(),
            "PutBucketLifecycle" => "PutBucketLifecycleConfiguration".to_string(),
            "GetBucketNotification" => "GetBucketNotificationConfiguration".to_string(),
            "PutBucketNotification" => "PutBucketNotificationConfiguration".to_string(),
            name => KEYED_CONFIGS
                .iter()
                .find(|k| {
                    name == format!("GetBucket{k}Configuration")
                        || name == format!("ListBucket{k}Configurations")
                })
                .map_or_else(
                    || name.to_string(),
                    |k| {
                        if has("id") {
                            format!("GetBucket{k}Configuration")
                        } else {
                            format!("ListBucket{k}Configurations")
                        }
                    },
                ),
        };
        model.operations.get(&name)
    }

    fn finish(&self, operation: &str, output: &Value, res: &mut Response) {
        match operation {
            "GetObject" if output.get("ContentRange").is_some() => res.status = 206,
            "GetBucketLocation" => {
                res.body = format!(
                    "<LocationConstraint xmlns=\"{XMLNS}\">{}</LocationConstraint>",
                    escape(output.str_field("LocationConstraint").unwrap_or_default())
                )
                .into_bytes();
                for (k, v) in &mut res.headers {
                    if k.eq_ignore_ascii_case("content-length") {
                        *v = res.body.len().to_string();
                    }
                }
            }
            _ => {}
        }
    }

    fn deliver(&mut self, ctx: &mut Ctx<'_>, resource: &str, delivery: &Delivery) -> bool {
        if let Some(arn) = Arn::parse(&delivery.source) {
            ctx.account = arn.account.to_string();
            ctx.region = arn.region.to_string();
        }
        let Some(key) = delivery.attributes.get("Key").and_then(Value::as_str) else {
            return false;
        };
        let body = delivery
            .bytes
            .as_deref()
            .unwrap_or(delivery.body.as_bytes());
        let input = json!({ "Bucket": resource, "Key": key, "Body": STANDARD.encode(body) });
        self.call(ctx, "PutObject", input).is_ok()
    }

    fn call(&mut self, ctx: &mut Ctx<'_>, operation: &str, input: Value) -> OpResult {
        let output = self.dispatch(ctx, operation, &input)?;
        if let Some(buckets) = self.accounts.get(&ctx.account) {
            notify::emit(buckets, ctx, operation, &input, &output);
        }
        Ok(output)
    }
}

impl S3 {
    fn dispatch(&mut self, ctx: &mut Ctx<'_>, operation: &str, input: &Value) -> OpResult {
        if matches!(operation, "CreateBucket" | "ListBuckets") {
            return bucket::account_op(&mut self.accounts, ctx, operation, input);
        }
        let buckets = self.accounts.entry(ctx.account.clone()).or_default();
        if let Some(result) = bucket::config_op(buckets, operation, input) {
            return result;
        }
        match operation {
            "HeadBucket"
            | "DeleteBucket"
            | "GetBucketLocation"
            | "PutBucketVersioning"
            | "GetBucketVersioning"
            | "GetBucketAcl"
            | "PutBucketAcl"
            | "GetBucketPolicyStatus" => bucket::bucket_op(buckets, ctx, operation, input),
            "ListObjects"
            | "ListObjectsV2"
            | "ListObjectVersions"
            | "ListMultipartUploads"
            | "ListParts" => listing::call(buckets, ctx, operation, input),
            "CreateMultipartUpload"
            | "UploadPart"
            | "UploadPartCopy"
            | "CompleteMultipartUpload"
            | "AbortMultipartUpload" => multipart::call(buckets, ctx, operation, input),
            _ => object::call(buckets, ctx, operation, input),
        }
    }
}

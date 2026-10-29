use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use super::bucket::acl_from;
use super::checksum::{Algorithm, Checksum, md5_hex};
use super::state::{Bucket, Object, attrs_from, tags_from_header};
use super::{
    Buckets, bucket_mut, bucket_ref, err, no_such_bucket, no_such_key, now_secs, owner, put,
    tag_set, tags_from, with,
};
use crate::codec::AwsError;
use crate::codec::rest::uri_decode;
use crate::codec::time::{format_http_date, value_epoch};
use crate::service::{Ctx, InputExt, OpResult};

const RESPONSE_OVERRIDES: &[(&str, &str)] = &[
    ("ResponseCacheControl", "CacheControl"),
    ("ResponseContentDisposition", "ContentDisposition"),
    ("ResponseContentEncoding", "ContentEncoding"),
    ("ResponseContentLanguage", "ContentLanguage"),
    ("ResponseContentType", "ContentType"),
    ("ResponseExpires", "Expires"),
];

pub fn body(input: &Value) -> Vec<u8> {
    input
        .get("Body")
        .and_then(Value::as_str)
        .and_then(|b| STANDARD.decode(b).ok())
        .unwrap_or_default()
}

/// Verifies `Content-MD5` and `x-amz-checksum-*` against `data` and returns
/// the checksum to store with it.
pub fn verify_upload(input: &Value, data: &[u8]) -> Result<Option<Checksum>, AwsError> {
    if let Some(md5) = input.str_field("ContentMD5") {
        let decoded = STANDARD
            .decode(md5)
            .ok()
            .filter(|d| d.len() == 16)
            .ok_or_else(|| {
                err(
                    400,
                    "InvalidDigest",
                    "The Content-MD5 you specified was invalid.",
                )
            })?;
        if super::checksum::hex(&decoded) != md5_hex(data) {
            return Err(err(
                400,
                "BadDigest",
                "The Content-MD5 you specified did not match what we received.",
            ));
        }
    }
    let mut stored = None;
    for alg in Algorithm::ALL {
        let Some(given) = input.str_field(alg.member()) else {
            continue;
        };
        let computed = alg.digest(data);
        match STANDARD.decode(given) {
            Ok(d) if d.len() == computed.len() => {}
            _ => {
                return Err(err(
                    400,
                    "InvalidRequest",
                    format!(
                        "Value for x-amz-checksum-{} header is invalid.",
                        alg.name().to_ascii_lowercase()
                    ),
                ));
            }
        }
        if STANDARD.encode(&computed) != given {
            return Err(err(
                400,
                "BadDigest",
                format!(
                    "The {} you specified did not match the calculated checksum.",
                    alg.name()
                ),
            ));
        }
        stored.get_or_insert(Checksum {
            algorithm: alg,
            value: given.to_string(),
            full_object: true,
        });
    }
    if stored.is_none()
        && let Some(alg) = input
            .str_field("ChecksumAlgorithm")
            .and_then(Algorithm::parse)
    {
        stored = Some(Checksum::of(alg, data));
    }
    Ok(stored)
}

fn checksum_fields(out: &mut Map<String, Value>, checksum: Option<&Checksum>) {
    if let Some(c) = checksum {
        out.insert(c.algorithm.member().into(), json!(c.value));
        out.insert("ChecksumType".into(), json!(c.kind()));
    }
}

/// Parses a `Range` header against an object of `size` bytes. `Ok(None)`
/// means the header is ignored and the whole object is returned.
pub fn parse_range(header: &str, size: usize) -> Result<Option<(usize, usize)>, ()> {
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return Ok(None);
    };
    if spec.contains(',') {
        return Ok(None);
    }
    let Some((start, end)) = spec.split_once('-') else {
        return Ok(None);
    };
    let num = |s: &str| s.trim().parse::<usize>();
    match (start.trim().is_empty(), end.trim().is_empty()) {
        (true, true) => Ok(None),
        (true, false) => {
            let Ok(n) = num(end) else { return Ok(None) };
            if n == 0 || size == 0 {
                return Err(());
            }
            Ok(Some((size.saturating_sub(n), size - 1)))
        }
        (false, open) => {
            let Ok(s) = num(start) else { return Ok(None) };
            let e = if open {
                usize::MAX
            } else {
                match num(end) {
                    Ok(e) => e,
                    Err(_) => return Ok(None),
                }
            };
            if e < s {
                return Ok(None);
            }
            if s >= size {
                return Err(());
            }
            Ok(Some((s, e.min(size - 1))))
        }
    }
}

fn etag_matches(header: &str, etag: &str) -> bool {
    let norm = |s: &str| {
        s.trim()
            .trim_start_matches("W/")
            .trim_matches('"')
            .to_string()
    };
    header
        .split(',')
        .any(|e| e.trim() == "*" || norm(e) == norm(etag))
}

fn precondition_failed(condition: &str) -> AwsError {
    with(
        err(
            412,
            "PreconditionFailed",
            "At least one of the pre-conditions you specified did not hold",
        ),
        "Condition",
        condition,
    )
}

fn not_modified() -> AwsError {
    err(304, "NotModified", "Not Modified")
}

/// Evaluates If-Match / If-Unmodified-Since (412) and If-None-Match /
/// If-Modified-Since (304, or 412 for copy sources) against `obj`.
pub fn check_conditions(input: &Value, obj: &Object, prefix: &str) -> Result<(), AwsError> {
    let field = |name: &str| format!("{prefix}{name}");
    let copy = !prefix.is_empty();
    let header = |name: &str| {
        if copy {
            format!("x-amz-copy-source-{}", name.to_ascii_lowercase())
        } else {
            name.to_string()
        }
    };
    let since = |name: &str| input.get(field(name)).and_then(value_epoch);
    if let Some(m) = input.str_field(&field("IfMatch")) {
        if !etag_matches(m, &obj.etag) {
            return Err(precondition_failed(&header("If-Match")));
        }
    } else if since("IfUnmodifiedSince").is_some_and(|t| obj.last_modified > t) {
        return Err(precondition_failed(&header("If-Unmodified-Since")));
    }
    let unchanged = match input.str_field(&field("IfNoneMatch")) {
        Some(m) => etag_matches(m, &obj.etag).then_some("If-None-Match"),
        None => since("IfModifiedSince")
            .is_some_and(|t| obj.last_modified <= t)
            .then_some("If-Modified-Since"),
    };
    match unchanged {
        Some(h) if copy => Err(precondition_failed(&header(h))),
        Some(_) => Err(not_modified()),
        None => Ok(()),
    }
}

/// Looks up the object a read addresses: the current version, or the
/// `VersionId` the request names.
fn read_target<'a>(
    bucket: &'a Bucket,
    key: &str,
    input: &Value,
    head: bool,
) -> Result<&'a Object, AwsError> {
    match input.str_field("VersionId") {
        Some(v) => {
            let obj = bucket.find(key, Some(v)).ok_or_else(|| {
                if head {
                    err(404, "NotFound", "Not Found")
                } else {
                    with(
                        with(
                            err(
                                404,
                                "NoSuchVersion",
                                "The specified version does not exist.",
                            ),
                            "Key",
                            key,
                        ),
                        "VersionId",
                        v,
                    )
                }
            })?;
            if obj.delete_marker {
                return Err(err(
                    405,
                    "MethodNotAllowed",
                    "The specified method is not allowed against this resource.",
                ));
            }
            Ok(obj)
        }
        None => bucket.current(key).ok_or_else(|| no_such_key(key)),
    }
}

fn describe(obj: &Object, input: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("AcceptRanges".into(), json!("bytes"));
    out.insert("ETag".into(), json!(obj.etag));
    out.insert("LastModified".into(), json!(obj.last_modified));
    out.insert("ContentType".into(), json!(obj.content_type));
    out.insert("ContentLength".into(), json!(obj.body.len()));
    for (k, v) in &obj.attrs {
        out.insert(k.clone(), v.clone());
    }
    if !obj.metadata.is_empty() {
        out.insert("Metadata".into(), Value::Object(obj.metadata.clone()));
    }
    put(&mut out, "VersionId", obj.version_id.clone());
    if obj.storage_class != "STANDARD" {
        out.insert("StorageClass".into(), json!(obj.storage_class));
    }
    if !obj.tags.is_empty() {
        out.insert("TagCount".into(), json!(obj.tags.len()));
    }
    for (param, member) in RESPONSE_OVERRIDES {
        if let Some(v) = input.get(*param).filter(|v| !v.is_null()) {
            let v = match (member, v) {
                (&"Expires", Value::Number(n)) => {
                    json!(format_http_date(n.as_f64().unwrap_or_default()))
                }
                _ => v.clone(),
            };
            out.insert(member.to_string(), v);
        }
    }
    out
}

fn get_object(bucket: &Bucket, input: &Value, head: bool) -> OpResult {
    let key = input.require_str("Key")?;
    let obj = read_target(bucket, key, input, head)?;
    check_conditions(input, obj, "")?;
    let mut out = describe(obj, input);
    let size = obj.body.len();
    let mut range = None;
    if let Some(n) = input.i64_field("PartNumber") {
        let invalid = || {
            err(
                416,
                "InvalidPartNumber",
                "The requested partnumber is not satisfiable",
            )
        };
        let parts: Vec<usize> = if obj.parts.is_empty() {
            vec![size]
        } else {
            obj.parts.iter().map(|p| p.size).collect()
        };
        let index = usize::try_from(n - 1).map_err(|_| invalid())?;
        let len = *parts.get(index).ok_or_else(invalid)?;
        let start: usize = parts[..index].iter().sum();
        out.insert("PartsCount".into(), json!(parts.len()));
        if len > 0 {
            range = Some((start, start + len - 1));
        }
    } else if let Some(h) = input.str_field("Range") {
        range = parse_range(h, size).map_err(|()| {
            with(
                with(
                    err(
                        416,
                        "InvalidRange",
                        "The requested range is not satisfiable",
                    ),
                    "RangeRequested",
                    h,
                ),
                "ActualObjectSize",
                size.to_string(),
            )
        })?;
    }
    let checksum_mode = input.str_field("ChecksumMode") == Some("ENABLED");
    match range {
        Some((s, e)) => {
            out.insert("ContentLength".into(), json!(e + 1 - s));
            out.insert(
                "ContentRange".into(),
                json!(format!("bytes {s}-{e}/{size}")),
            );
            if !head {
                out.insert("Body".into(), json!(STANDARD.encode(&obj.body[s..=e])));
            }
        }
        None => {
            if checksum_mode {
                checksum_fields(&mut out, obj.checksum.as_ref());
            }
            if !head {
                out.insert("Body".into(), json!(STANDARD.encode(&obj.body)));
            }
        }
    }
    if head && checksum_mode {
        checksum_fields(&mut out, obj.checksum.as_ref());
    }
    Ok(Value::Object(out))
}

fn require_lock(bucket: &Bucket) -> Result<(), AwsError> {
    if bucket.configs.contains_key("ObjectLockConfiguration") {
        Ok(())
    } else {
        Err(err(
            400,
            "InvalidRequest",
            "Bucket is missing Object Lock Configuration",
        ))
    }
}

fn put_object(bucket: &mut Bucket, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let key = input.require_str("Key")?;
    let current = bucket.current(key);
    if let Some(m) = input.str_field("IfNoneMatch")
        && current.is_some_and(|c| etag_matches(m, &c.etag))
    {
        return Err(precondition_failed("If-None-Match"));
    }
    if let Some(m) = input.str_field("IfMatch") {
        let c = current.ok_or_else(|| no_such_key(key))?;
        if !etag_matches(m, &c.etag) {
            return Err(precondition_failed("If-Match"));
        }
    }
    let data = body(input);
    let checksum =
        verify_upload(input, &data)?.unwrap_or_else(|| Checksum::of(Algorithm::Crc64Nvme, &data));
    let attrs = attrs_from(input);
    if attrs.contains_key("ObjectLockMode") || attrs.contains_key("ObjectLockLegalHoldStatus") {
        require_lock(bucket)?;
    }
    let obj = Object {
        version_id: None,
        delete_marker: false,
        etag: format!("\"{}\"", md5_hex(&data)),
        last_modified: now_secs(ctx),
        content_type: input
            .str_field("ContentType")
            .unwrap_or("binary/octet-stream")
            .to_string(),
        metadata: input.obj_field("Metadata").cloned().unwrap_or_default(),
        storage_class: input
            .str_field("StorageClass")
            .unwrap_or("STANDARD")
            .to_string(),
        tags: tags_from_header(input.str_field("Tagging")),
        acl: acl_from(ctx, input)?,
        checksum: Some(checksum.clone()),
        parts: Vec::new(),
        body: data,
        attrs,
    };
    let mut out = Map::new();
    out.insert("ETag".into(), json!(obj.etag));
    out.insert("Size".into(), json!(obj.body.len()));
    for k in ["ServerSideEncryption", "SSEKMSKeyId", "BucketKeyEnabled"] {
        put(&mut out, k, obj.attrs.get(k).cloned());
    }
    checksum_fields(&mut out, Some(&checksum));
    put(&mut out, "VersionId", bucket.insert(ctx, key, obj));
    Ok(Value::Object(out))
}

/// Splits `x-amz-copy-source` (`bucket/key?versionId=...`, URL-encoded).
pub fn copy_source(input: &Value) -> Result<(String, String, Option<String>), AwsError> {
    let raw = input.require_str("CopySource")?;
    let (path, query) = raw.split_once('?').unwrap_or((raw, ""));
    let path = uri_decode(path.trim_start_matches('/'));
    let invalid =
        || super::invalid_argument("Invalid copy source object key", "x-amz-copy-source", raw);
    let (bucket, key) = path.split_once('/').ok_or_else(invalid)?;
    if bucket.is_empty() || key.is_empty() {
        return Err(invalid());
    }
    let version = crate::codec::rest::parse_query(query)
        .into_iter()
        .find(|(k, _)| k == "versionId")
        .map(|(_, v)| v);
    Ok((bucket.to_string(), key.to_string(), version))
}

pub fn source_object(
    buckets: &Buckets,
    input: &Value,
) -> Result<(Object, Option<String>), AwsError> {
    let (bucket, key, version) = copy_source(input)?;
    let src = buckets
        .get(&bucket)
        .ok_or_else(|| no_such_bucket(&bucket))?;
    let obj = match &version {
        Some(v) => {
            let o = src.find(&key, Some(v)).ok_or_else(|| {
                with(
                    err(
                        404,
                        "NoSuchVersion",
                        "The specified version does not exist.",
                    ),
                    "VersionId",
                    v.as_str(),
                )
            })?;
            if o.delete_marker {
                return Err(err(
                    400,
                    "InvalidRequest",
                    "The source of a copy request may not specifically refer to a delete marker by version id.",
                ));
            }
            o
        }
        None => src.current(&key).ok_or_else(|| no_such_key(&key))?,
    };
    check_conditions(input, obj, "CopySource")?;
    let same = input.str_field("Bucket") == Some(bucket.as_str())
        && input.str_field("Key") == Some(key.as_str());
    let source_version = obj.version_id.clone();
    if same && version.is_none() {
        let changes = input.str_field("MetadataDirective") == Some("REPLACE")
            || input.str_field("TaggingDirective") == Some("REPLACE")
            || [
                "StorageClass",
                "WebsiteRedirectLocation",
                "ServerSideEncryption",
                "ChecksumAlgorithm",
            ]
            .iter()
            .any(|k| input.str_field(k).is_some());
        if !changes {
            return Err(err(
                400,
                "InvalidRequest",
                "This copy request is illegal because it is trying to copy an object to itself without changing the object's metadata, storage class, website redirect location or encryption attributes.",
            ));
        }
    }
    Ok((obj.clone(), source_version))
}

fn copy_object(buckets: &mut Buckets, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let key = input.require_str("Key")?.to_string();
    bucket_ref(buckets, input)?;
    let (src, source_version) = source_object(buckets, input)?;
    let replace = input.str_field("MetadataDirective") == Some("REPLACE");
    let mut obj = src;
    obj.last_modified = now_secs(ctx);
    if replace {
        obj.content_type = input
            .str_field("ContentType")
            .unwrap_or("binary/octet-stream")
            .to_string();
        obj.metadata = input.obj_field("Metadata").cloned().unwrap_or_default();
        obj.attrs = attrs_from(input);
    } else {
        for k in [
            "ServerSideEncryption",
            "SSEKMSKeyId",
            "BucketKeyEnabled",
            "WebsiteRedirectLocation",
        ] {
            if let Some(v) = input.get(k).filter(|v| !v.is_null()) {
                obj.attrs.insert(k.into(), v.clone());
            }
        }
    }
    if input.str_field("TaggingDirective") == Some("REPLACE") {
        obj.tags = tags_from_header(input.str_field("Tagging"));
    }
    if let Some(sc) = input.str_field("StorageClass") {
        obj.storage_class = sc.to_string();
    }
    if let Some(alg) = input
        .str_field("ChecksumAlgorithm")
        .and_then(Algorithm::parse)
    {
        obj.checksum = Some(Checksum::of(alg, &obj.body));
    }
    obj.acl = acl_from(ctx, input)?;
    let mut result = Map::new();
    result.insert("ETag".into(), json!(obj.etag));
    result.insert("LastModified".into(), json!(obj.last_modified));
    checksum_fields(&mut result, obj.checksum.as_ref());
    let mut out = Map::new();
    out.insert(
        "ServerSideEncryption".into(),
        obj.attrs
            .get("ServerSideEncryption")
            .cloned()
            .unwrap_or(json!("AES256")),
    );
    let bucket = bucket_mut(buckets, input)?;
    put(&mut out, "VersionId", bucket.insert(ctx, &key, obj));
    put(&mut out, "CopySourceVersionId", source_version);
    out.insert("CopyObjectResult".into(), Value::Object(result));
    Ok(Value::Object(out))
}

fn bypass(input: &Value) -> bool {
    input
        .get("BypassGovernanceRetention")
        .and_then(Value::as_bool)
        == Some(true)
}

fn delete_objects(bucket: &mut Bucket, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let delete = input.obj_field("Delete");
    let objects = delete
        .and_then(|d| d.get("Objects"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if objects.is_empty() || objects.len() > 1000 {
        return Err(err(
            400,
            "MalformedXML",
            "The XML you provided was not well-formed or did not validate against our published schema",
        ));
    }
    let quiet = delete.and_then(|d| d.get("Quiet")).and_then(Value::as_bool) == Some(true);
    let mut deleted = Vec::new();
    let mut errors = Vec::new();
    for o in &objects {
        let key = o.str_field("Key").unwrap_or_default();
        let version = o.str_field("VersionId");
        match bucket.delete(ctx, key, version, bypass(input)) {
            Ok(d) => {
                let mut entry = Map::new();
                entry.insert("Key".into(), json!(key));
                match version {
                    Some(v) => {
                        entry.insert("VersionId".into(), json!(v));
                        if d.delete_marker {
                            entry.insert("DeleteMarker".into(), json!(true));
                            entry.insert("DeleteMarkerVersionId".into(), json!(v));
                        }
                    }
                    None if d.delete_marker => {
                        entry.insert("DeleteMarker".into(), json!(true));
                        put(&mut entry, "DeleteMarkerVersionId", d.version_id);
                    }
                    None => {}
                }
                if !quiet {
                    deleted.push(Value::Object(entry));
                }
            }
            Err(e) => {
                let mut entry = Map::new();
                entry.insert("Key".into(), json!(key));
                put(&mut entry, "VersionId", version);
                entry.insert("Code".into(), json!(e.code));
                entry.insert("Message".into(), json!(e.message));
                errors.push(Value::Object(entry));
            }
        }
    }
    Ok(json!({ "Deleted": deleted, "Errors": errors }))
}

fn attributes(bucket: &Bucket, input: &Value) -> OpResult {
    let key = input.require_str("Key")?;
    let obj = read_target(bucket, key, input, false)?;
    let wanted: Vec<&str> = input
        .array_field("ObjectAttributes")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let mut out = Map::new();
    out.insert("LastModified".into(), json!(obj.last_modified));
    put(&mut out, "VersionId", obj.version_id.clone());
    if wanted.contains(&"ETag") {
        out.insert("ETag".into(), json!(obj.etag.trim_matches('"')));
    }
    if wanted.contains(&"ObjectSize") {
        out.insert("ObjectSize".into(), json!(obj.body.len()));
    }
    if wanted.contains(&"StorageClass") {
        out.insert("StorageClass".into(), json!(obj.storage_class));
    }
    if wanted.contains(&"Checksum")
        && let Some(c) = &obj.checksum
    {
        let mut cs = Map::new();
        cs.insert(
            c.algorithm.member().into(),
            json!(c.value.split('-').next().unwrap_or_default()),
        );
        cs.insert("ChecksumType".into(), json!(c.kind()));
        out.insert("Checksum".into(), Value::Object(cs));
    }
    if wanted.contains(&"ObjectParts") && !obj.parts.is_empty() {
        let marker = input.i64_field("PartNumberMarker").unwrap_or(0).max(0) as usize;
        let max = input.i64_field("MaxParts").unwrap_or(1000).clamp(0, 1000) as usize;
        let algorithm = obj.checksum.as_ref().map(|c| c.algorithm);
        let parts: Vec<Value> = obj
            .parts
            .iter()
            .enumerate()
            .skip(marker)
            .take(max)
            .map(|(i, p)| {
                let mut part = Map::new();
                part.insert("PartNumber".into(), json!(i + 1));
                part.insert("Size".into(), json!(p.size));
                if let (Some(a), Some(c)) = (algorithm, &p.checksum) {
                    part.insert(a.member().into(), json!(c));
                }
                Value::Object(part)
            })
            .collect();
        let next = marker + parts.len();
        out.insert(
            "ObjectParts".into(),
            json!({
                "TotalPartsCount": obj.parts.len(),
                "PartNumberMarker": marker,
                "NextPartNumberMarker": next,
                "MaxParts": max,
                "IsTruncated": next < obj.parts.len(),
                "Parts": parts,
            }),
        );
    }
    Ok(Value::Object(out))
}

fn target_mut<'a>(bucket: &'a mut Bucket, input: &Value) -> Result<&'a mut Object, AwsError> {
    let key = input.require_str("Key")?;
    let version = input.str_field("VersionId");
    match bucket.find_mut(key, version) {
        Some(o) if !o.delete_marker => Ok(o),
        Some(_) => Err(err(
            405,
            "MethodNotAllowed",
            "The specified method is not allowed against this resource.",
        )),
        None if version.is_some() => Err(with(
            err(
                404,
                "NoSuchVersion",
                "The specified version does not exist.",
            ),
            "Key",
            key,
        )),
        None => Err(no_such_key(key)),
    }
}

fn no_lock_config() -> AwsError {
    err(
        404,
        "NoSuchObjectLockConfiguration",
        "The specified object does not have a ObjectLock configuration",
    )
}

pub fn call(buckets: &mut Buckets, ctx: &mut Ctx<'_>, operation: &str, input: &Value) -> OpResult {
    match operation {
        "CopyObject" => return copy_object(buckets, ctx, input),
        "GetObject" | "HeadObject" => {
            return get_object(
                bucket_ref(buckets, input)?,
                input,
                operation == "HeadObject",
            );
        }
        "GetObjectAttributes" => return attributes(bucket_ref(buckets, input)?, input),
        _ => {}
    }
    let bucket = bucket_mut(buckets, input)?;
    match operation {
        "PutObject" => put_object(bucket, ctx, input),
        "DeleteObject" => {
            let key = input.require_str("Key")?;
            let d = bucket.delete(ctx, key, input.str_field("VersionId"), bypass(input))?;
            let mut out = Map::new();
            if d.delete_marker {
                out.insert("DeleteMarker".into(), json!(true));
            }
            put(&mut out, "VersionId", d.version_id);
            Ok(Value::Object(out))
        }
        "DeleteObjects" => delete_objects(bucket, ctx, input),
        "PutObjectTagging" => {
            let obj = target_mut(bucket, input)?;
            obj.tags = tags_from(input.obj_field("Tagging"));
            let mut out = Map::new();
            put(&mut out, "VersionId", obj.version_id.clone());
            Ok(Value::Object(out))
        }
        "GetObjectTagging" => {
            let obj = target_mut(bucket, input)?;
            let mut out = Map::new();
            out.insert("TagSet".into(), tag_set(&obj.tags));
            put(&mut out, "VersionId", obj.version_id.clone());
            Ok(Value::Object(out))
        }
        "DeleteObjectTagging" => {
            let obj = target_mut(bucket, input)?;
            obj.tags.clear();
            let mut out = Map::new();
            put(&mut out, "VersionId", obj.version_id.clone());
            Ok(Value::Object(out))
        }
        "GetObjectAcl" => {
            let obj = target_mut(bucket, input)?;
            Ok(json!({ "Owner": owner(ctx), "Grants": obj.acl }))
        }
        "PutObjectAcl" => {
            let acl = acl_from(ctx, input)?;
            target_mut(bucket, input)?.acl = acl;
            Ok(json!({}))
        }
        "PutObjectRetention" => {
            require_lock(bucket)?;
            let retention = input.obj_field("Retention").cloned().unwrap_or_default();
            let now = now_secs(ctx);
            let bypass = bypass(input);
            let obj = target_mut(bucket, input)?;
            let current_until = obj
                .attrs
                .get("ObjectLockRetainUntilDate")
                .and_then(value_epoch);
            let new_until = retention.get("RetainUntilDate").and_then(value_epoch);
            let shortened =
                current_until.is_some_and(|c| c > now && new_until.is_none_or(|n| n < c));
            match obj.attrs.get("ObjectLockMode").and_then(Value::as_str) {
                Some("COMPLIANCE") if shortened => return Err(super::access_denied()),
                Some("GOVERNANCE") if shortened && !bypass => return Err(super::access_denied()),
                _ => {}
            }
            for (from, to) in [
                ("Mode", "ObjectLockMode"),
                ("RetainUntilDate", "ObjectLockRetainUntilDate"),
            ] {
                match retention.get(from) {
                    Some(v) => obj.attrs.insert(to.into(), v.clone()),
                    None => obj.attrs.remove(to),
                };
            }
            Ok(json!({}))
        }
        "GetObjectRetention" => {
            require_lock(bucket)?;
            let obj = target_mut(bucket, input)?;
            let mode = obj.attrs.get("ObjectLockMode").ok_or_else(no_lock_config)?;
            Ok(json!({ "Retention": {
                "Mode": mode,
                "RetainUntilDate": obj.attrs.get("ObjectLockRetainUntilDate"),
            } }))
        }
        "PutObjectLegalHold" => {
            require_lock(bucket)?;
            let status = input
                .obj_field("LegalHold")
                .and_then(|h| h.get("Status"))
                .cloned()
                .unwrap_or(json!("OFF"));
            target_mut(bucket, input)?
                .attrs
                .insert("ObjectLockLegalHoldStatus".into(), status);
            Ok(json!({}))
        }
        "GetObjectLegalHold" => {
            require_lock(bucket)?;
            let obj = target_mut(bucket, input)?;
            let status = obj
                .attrs
                .get("ObjectLockLegalHoldStatus")
                .ok_or_else(no_lock_config)?;
            Ok(json!({ "LegalHold": { "Status": status } }))
        }
        _ => Err(err(
            501,
            "NotImplemented",
            format!("wasmo: {operation} is not implemented"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-4", 10), Ok(Some((0, 4))));
        assert_eq!(parse_range("bytes=5-", 10), Ok(Some((5, 9))));
        assert_eq!(parse_range("bytes=-3", 10), Ok(Some((7, 9))));
        assert_eq!(parse_range("bytes=-30", 10), Ok(Some((0, 9))));
        assert_eq!(parse_range("bytes=8-100", 10), Ok(Some((8, 9))));
        assert_eq!(parse_range("bytes=10-20", 10), Err(()));
        assert_eq!(parse_range("bytes=-0", 10), Err(()));
        assert_eq!(parse_range("bytes=0-0", 0), Err(()));
        assert_eq!(parse_range("bytes=5-2", 10), Ok(None));
        assert_eq!(parse_range("bytes=0-1,4-5", 10), Ok(None));
        assert_eq!(parse_range("items=0-1", 10), Ok(None));
        assert_eq!(parse_range("bytes=x-1", 10), Ok(None));
    }

    #[test]
    fn copy_source_is_url_decoded() {
        let input = json!({ "CopySource": "/src/dir%2Fa%20b%E3%81%82.txt?versionId=v1" });
        assert_eq!(
            copy_source(&input).unwrap(),
            ("src".into(), "dir/a bあ.txt".into(), Some("v1".into()))
        );
        assert!(copy_source(&json!({ "CopySource": "bucket-only" })).is_err());
    }

    #[test]
    fn etags_compare_without_quotes_and_weak_prefix() {
        assert!(etag_matches("\"abc\"", "\"abc\""));
        assert!(etag_matches("W/\"abc\", \"x\"", "\"abc\""));
        assert!(etag_matches("*", "\"abc\""));
        assert!(!etag_matches("\"abd\"", "\"abc\""));
    }

    #[test]
    fn verifies_upload_checksums() {
        let ok = json!({ "ChecksumCRC32": "DUoRhQ==" });
        let c = verify_upload(&ok, b"hello world").unwrap().unwrap();
        assert_eq!(c.algorithm, Algorithm::Crc32);
        let bad = json!({ "ChecksumCRC32": "AAAAAA==" });
        assert_eq!(
            verify_upload(&bad, b"hello world").unwrap_err().code,
            "BadDigest"
        );
        let md5 = json!({ "ContentMD5": "XrY7u+Ae7tCTyyK7j1rNww==" });
        assert!(verify_upload(&md5, b"hello world").unwrap().is_none());
        let wrong_md5 = json!({ "ContentMD5": "XrY7u+Ae7tCTyyK7j1rNwA==" });
        assert_eq!(
            verify_upload(&wrong_md5, b"hello world").unwrap_err().code,
            "BadDigest"
        );
    }
}

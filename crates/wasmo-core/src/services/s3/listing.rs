use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use super::list::{Last, page};
use super::state::Object;
use super::{Buckets, bucket_ref, encode_key, err, no_such_upload, owner, put};
use crate::codec::AwsError;
use crate::service::{Ctx, InputExt, OpResult};

fn max_keys(input: &Value, field: &str) -> usize {
    input
        .i64_field(field)
        .map_or(1000, |m| m.clamp(0, 1000) as usize)
}

fn url_encoding(input: &Value) -> Result<bool, AwsError> {
    match input.str_field("EncodingType") {
        None => Ok(false),
        Some("url") => Ok(true),
        Some(other) => Err(super::invalid_argument(
            "Invalid Encoding Method specified in Request",
            "encoding-type",
            other,
        )),
    }
}

fn entry(key: &str, obj: &Object, url: bool, owner: Option<&Value>) -> Value {
    let mut e = Map::new();
    e.insert("Key".into(), json!(encode_key(key, url)));
    e.insert("LastModified".into(), json!(obj.last_modified));
    e.insert("ETag".into(), json!(obj.etag));
    e.insert("Size".into(), json!(obj.body.len()));
    e.insert("StorageClass".into(), json!(obj.storage_class));
    if let Some(c) = &obj.checksum {
        e.insert("ChecksumAlgorithm".into(), json!([c.algorithm.name()]));
        e.insert("ChecksumType".into(), json!(c.kind()));
    }
    put(&mut e, "Owner", owner.cloned());
    Value::Object(e)
}

fn prefixes(list: &[String], url: bool) -> Value {
    Value::Array(
        list.iter()
            .map(|p| json!({ "Prefix": encode_key(p, url) }))
            .collect(),
    )
}

fn list_objects(buckets: &Buckets, ctx: &Ctx<'_>, operation: &str, input: &Value) -> OpResult {
    let bucket = bucket_ref(buckets, input)?;
    let v2 = operation == "ListObjectsV2";
    let url = url_encoding(input)?;
    let prefix = input.str_field("Prefix").unwrap_or_default();
    let delimiter = input.str_field("Delimiter").unwrap_or_default();
    let max = max_keys(input, "MaxKeys");
    let after = if v2 {
        match input.str_field("ContinuationToken") {
            Some(t) => STANDARD
                .decode(t)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
                .ok_or_else(|| {
                    super::invalid_argument(
                        "The continuation token provided is incorrect",
                        "continuation-token",
                        t,
                    )
                })?,
            None => input
                .str_field("StartAfter")
                .unwrap_or_default()
                .to_string(),
        }
    } else {
        input.str_field("Marker").unwrap_or_default().to_string()
    };
    let items = bucket
        .objects
        .range::<str, _>((
            std::ops::Bound::Excluded(after.as_str()),
            std::ops::Bound::Unbounded,
        ))
        .filter_map(|(k, versions)| {
            versions
                .first()
                .filter(|o| !o.delete_marker)
                .map(|o| (k.as_str(), (k.as_str(), o)))
        });
    let p = page(items, prefix, delimiter, &after, max);
    let next = match p.last {
        Some(Last::Entry) => p.entries.last().map(|(k, _)| k.to_string()),
        Some(Last::Prefix) => p.prefixes.last().cloned(),
        None => None,
    }
    .filter(|_| p.truncated);
    let fetch_owner = !v2 || input.get("FetchOwner").and_then(Value::as_bool) == Some(true);
    let owner = fetch_owner.then(|| owner(ctx));
    let mut out = Map::new();
    out.insert("Name".into(), json!(input.str_field("Bucket")));
    out.insert("Prefix".into(), json!(encode_key(prefix, url)));
    if !delimiter.is_empty() {
        out.insert("Delimiter".into(), json!(encode_key(delimiter, url)));
    }
    out.insert("MaxKeys".into(), json!(max));
    out.insert("IsTruncated".into(), json!(p.truncated));
    if url {
        out.insert("EncodingType".into(), json!("url"));
    }
    out.insert(
        "Contents".into(),
        Value::Array(
            p.entries
                .iter()
                .map(|(k, o)| entry(k, o, url, owner.as_ref()))
                .collect(),
        ),
    );
    out.insert("CommonPrefixes".into(), prefixes(&p.prefixes, url));
    if v2 {
        out.insert("KeyCount".into(), json!(p.count()));
        put(
            &mut out,
            "ContinuationToken",
            input.str_field("ContinuationToken"),
        );
        put(
            &mut out,
            "StartAfter",
            input.str_field("StartAfter").map(|s| encode_key(s, url)),
        );
        put(
            &mut out,
            "NextContinuationToken",
            next.map(|n| STANDARD.encode(n)),
        );
    } else {
        out.insert("Marker".into(), json!(encode_key(&after, url)));
        put(&mut out, "NextMarker", next.map(|n| encode_key(&n, url)));
    }
    Ok(Value::Object(out))
}

fn list_versions(buckets: &Buckets, ctx: &Ctx<'_>, input: &Value) -> OpResult {
    let bucket = bucket_ref(buckets, input)?;
    let url = url_encoding(input)?;
    let prefix = input.str_field("Prefix").unwrap_or_default();
    let delimiter = input.str_field("Delimiter").unwrap_or_default();
    let max = max_keys(input, "MaxKeys");
    let key_marker = input.str_field("KeyMarker").unwrap_or_default();
    let version_marker = input.str_field("VersionIdMarker");
    let items = bucket.objects.iter().flat_map(|(k, versions)| {
        let skip = match (k.as_str().cmp(key_marker), version_marker) {
            (std::cmp::Ordering::Greater, _) => Some(0),
            (std::cmp::Ordering::Equal, Some(v)) => versions
                .iter()
                .position(|o| o.version_label() == v)
                .map(|i| i + 1),
            _ => None,
        };
        versions
            .iter()
            .enumerate()
            .skip(skip.unwrap_or(versions.len()))
            .map(move |(i, o)| (k.as_str(), (k.as_str(), i == 0, o)))
    });
    let after = if version_marker.is_some() {
        ""
    } else {
        key_marker
    };
    let p = page(items, prefix, delimiter, after, max);
    let owner = owner(ctx);
    let mut versions = Vec::new();
    let mut markers = Vec::new();
    for (key, latest, o) in &p.entries {
        let mut e = Map::new();
        e.insert("Key".into(), json!(encode_key(key, url)));
        e.insert("VersionId".into(), json!(o.version_label()));
        e.insert("IsLatest".into(), json!(latest));
        e.insert("LastModified".into(), json!(o.last_modified));
        e.insert("Owner".into(), owner.clone());
        if o.delete_marker {
            markers.push(Value::Object(e));
        } else {
            e.insert("ETag".into(), json!(o.etag));
            e.insert("Size".into(), json!(o.body.len()));
            e.insert("StorageClass".into(), json!(o.storage_class));
            if let Some(c) = &o.checksum {
                e.insert("ChecksumAlgorithm".into(), json!([c.algorithm.name()]));
                e.insert("ChecksumType".into(), json!(c.kind()));
            }
            versions.push(Value::Object(e));
        }
    }
    let mut out = Map::new();
    out.insert("Name".into(), json!(input.str_field("Bucket")));
    out.insert("Prefix".into(), json!(encode_key(prefix, url)));
    if !delimiter.is_empty() {
        out.insert("Delimiter".into(), json!(encode_key(delimiter, url)));
    }
    out.insert("KeyMarker".into(), json!(encode_key(key_marker, url)));
    put(&mut out, "VersionIdMarker", version_marker);
    out.insert("MaxKeys".into(), json!(max));
    out.insert("IsTruncated".into(), json!(p.truncated));
    if url {
        out.insert("EncodingType".into(), json!("url"));
    }
    if p.truncated {
        match p.last {
            Some(Last::Entry) => {
                let (k, _, o) = p.entries.last().expect("last entry");
                out.insert("NextKeyMarker".into(), json!(encode_key(k, url)));
                out.insert("NextVersionIdMarker".into(), json!(o.version_label()));
            }
            Some(Last::Prefix) => {
                let pre = p.prefixes.last().expect("last prefix");
                out.insert("NextKeyMarker".into(), json!(encode_key(pre, url)));
            }
            None => {}
        }
    }
    out.insert("Versions".into(), Value::Array(versions));
    out.insert("DeleteMarkers".into(), Value::Array(markers));
    out.insert("CommonPrefixes".into(), prefixes(&p.prefixes, url));
    Ok(Value::Object(out))
}

fn list_uploads(buckets: &Buckets, ctx: &Ctx<'_>, input: &Value) -> OpResult {
    let bucket = bucket_ref(buckets, input)?;
    let url = url_encoding(input)?;
    let prefix = input.str_field("Prefix").unwrap_or_default();
    let delimiter = input.str_field("Delimiter").unwrap_or_default();
    let max = max_keys(input, "MaxUploads");
    let key_marker = input.str_field("KeyMarker").unwrap_or_default();
    let id_marker = input.str_field("UploadIdMarker");
    let mut uploads: Vec<_> = bucket.uploads.iter().collect();
    uploads.sort_by(|(ia, a), (ib, b)| {
        (&a.key, a.initiated, ia)
            .partial_cmp(&(&b.key, b.initiated, ib))
            .expect("finite times")
    });
    let mut started = id_marker.is_none();
    let items = uploads
        .into_iter()
        .filter(|(id, u)| {
            if u.key.as_str() > key_marker {
                return true;
            }
            if u.key == key_marker && id_marker.is_some() {
                if started {
                    return true;
                }
                started = Some(id.as_str()) == id_marker;
            }
            false
        })
        .map(|(id, u)| (u.key.as_str(), (id, u)));
    let after = if id_marker.is_some() { "" } else { key_marker };
    let p = page(items, prefix, delimiter, after, max);
    let owner = owner(ctx);
    let mut out = Map::new();
    out.insert("Bucket".into(), json!(input.str_field("Bucket")));
    out.insert("Prefix".into(), json!(encode_key(prefix, url)));
    if !delimiter.is_empty() {
        out.insert("Delimiter".into(), json!(encode_key(delimiter, url)));
    }
    out.insert("KeyMarker".into(), json!(encode_key(key_marker, url)));
    put(&mut out, "UploadIdMarker", id_marker);
    out.insert("MaxUploads".into(), json!(max));
    out.insert("IsTruncated".into(), json!(p.truncated));
    if url {
        out.insert("EncodingType".into(), json!("url"));
    }
    if p.truncated {
        match p.last {
            Some(Last::Entry) => {
                let (id, u) = p.entries.last().expect("last entry");
                out.insert("NextKeyMarker".into(), json!(encode_key(&u.key, url)));
                out.insert("NextUploadIdMarker".into(), json!(id));
            }
            Some(Last::Prefix) => {
                let pre = p.prefixes.last().expect("last prefix");
                out.insert("NextKeyMarker".into(), json!(encode_key(pre, url)));
            }
            None => {}
        }
    }
    out.insert(
        "Uploads".into(),
        Value::Array(
            p.entries
                .iter()
                .map(|(id, u)| {
                    let mut e = Map::new();
                    e.insert("UploadId".into(), json!(id));
                    e.insert("Key".into(), json!(encode_key(&u.key, url)));
                    e.insert("Initiated".into(), json!(u.initiated));
                    e.insert("StorageClass".into(), json!(u.storage_class));
                    e.insert("Owner".into(), owner.clone());
                    e.insert("Initiator".into(), owner.clone());
                    if let Some(a) = u.algorithm {
                        e.insert("ChecksumAlgorithm".into(), json!(a.name()));
                        e.insert(
                            "ChecksumType".into(),
                            json!(if u.full_object {
                                "FULL_OBJECT"
                            } else {
                                "COMPOSITE"
                            }),
                        );
                    }
                    Value::Object(e)
                })
                .collect(),
        ),
    );
    out.insert("CommonPrefixes".into(), prefixes(&p.prefixes, url));
    Ok(Value::Object(out))
}

fn list_parts(buckets: &Buckets, ctx: &Ctx<'_>, input: &Value) -> OpResult {
    let bucket = bucket_ref(buckets, input)?;
    let id = input.require_str("UploadId")?;
    let key = input.require_str("Key")?;
    let upload = bucket
        .uploads
        .get(id)
        .filter(|u| u.key == key)
        .ok_or_else(|| no_such_upload(id))?;
    let marker = input.i64_field("PartNumberMarker").unwrap_or(0).max(0) as u32;
    let max = max_keys(input, "MaxParts");
    let mut remaining = upload.parts.range(marker + 1..).peekable();
    let mut parts = Vec::new();
    let mut next = None;
    while parts.len() < max {
        let Some((n, p)) = remaining.next() else {
            break;
        };
        let mut e = Map::new();
        e.insert("PartNumber".into(), json!(n));
        e.insert("LastModified".into(), json!(p.last_modified));
        e.insert("ETag".into(), json!(p.etag));
        e.insert("Size".into(), json!(p.body.len()));
        if let (Some(a), Some(c)) = (upload.algorithm, &p.checksum) {
            e.insert(a.member().into(), json!(c));
        }
        parts.push(Value::Object(e));
        next = Some(*n);
    }
    let truncated = remaining.peek().is_some();
    let owner = owner(ctx);
    let mut out = Map::new();
    out.insert("Bucket".into(), json!(input.str_field("Bucket")));
    out.insert("Key".into(), json!(key));
    out.insert("UploadId".into(), json!(id));
    out.insert("PartNumberMarker".into(), json!(marker));
    put(&mut out, "NextPartNumberMarker", next);
    out.insert("MaxParts".into(), json!(max));
    out.insert("IsTruncated".into(), json!(truncated));
    out.insert("Parts".into(), Value::Array(parts));
    out.insert("Owner".into(), owner.clone());
    out.insert("Initiator".into(), owner);
    out.insert("StorageClass".into(), json!(upload.storage_class));
    if let Some(a) = upload.algorithm {
        out.insert("ChecksumAlgorithm".into(), json!(a.name()));
        out.insert(
            "ChecksumType".into(),
            json!(if upload.full_object {
                "FULL_OBJECT"
            } else {
                "COMPOSITE"
            }),
        );
    }
    Ok(Value::Object(out))
}

pub fn call(buckets: &mut Buckets, ctx: &mut Ctx<'_>, operation: &str, input: &Value) -> OpResult {
    match operation {
        "ListObjects" | "ListObjectsV2" => list_objects(buckets, ctx, operation, input),
        "ListObjectVersions" => list_versions(buckets, ctx, input),
        "ListMultipartUploads" => list_uploads(buckets, ctx, input),
        "ListParts" => list_parts(buckets, ctx, input),
        _ => Err(err(
            501,
            "NotImplemented",
            format!("wasmo: {operation} is not implemented"),
        )),
    }
}

use serde_json::{Map, Value, json};

use super::bucket::acl_from;
use super::checksum::{Algorithm, Checksum, composite, md5_hex, multipart_etag};
use super::object::{body, parse_range, source_object, verify_upload};
use super::state::{Object, Part, PartInfo, Upload, attrs_from, tags_from_header};
use super::{Buckets, bucket_mut, err, no_such_upload, now_secs, put, with};
use crate::codec::AwsError;
use crate::service::{Ctx, InputExt, OpResult};

const MIN_PART_SIZE: usize = 5 * 1024 * 1024;

fn part_number(input: &Value) -> Result<u32, AwsError> {
    input
        .i64_field("PartNumber")
        .filter(|n| (1..=10000).contains(n))
        .map(|n| n as u32)
        .ok_or_else(|| {
            super::invalid_argument(
                "Part number must be an integer between 1 and 10000, inclusive",
                "partNumber",
                &input
                    .get("PartNumber")
                    .map(Value::to_string)
                    .unwrap_or_default(),
            )
        })
}

fn upload_mut<'a>(buckets: &'a mut Buckets, input: &Value) -> Result<&'a mut Upload, AwsError> {
    let id = input.require_str("UploadId")?;
    let key = input.require_str("Key")?;
    bucket_mut(buckets, input)?
        .uploads
        .get_mut(id)
        .filter(|u| u.key == key)
        .ok_or_else(|| no_such_upload(id))
}

fn store_part(
    ctx: &Ctx<'_>,
    upload: &mut Upload,
    number: u32,
    data: Vec<u8>,
    given: Option<Checksum>,
) -> Part {
    let checksum = upload.algorithm.map(|alg| match given {
        Some(c) if c.algorithm == alg => c.value,
        _ => alg.encode(&data),
    });
    let part = Part {
        etag: format!("\"{}\"", md5_hex(&data)),
        last_modified: now_secs(ctx),
        checksum,
        body: data,
    };
    upload.parts.insert(number, part.clone());
    part
}

fn invalid_part() -> AwsError {
    err(
        400,
        "InvalidPart",
        "One or more of the specified parts could not be found.  The part may not have been uploaded, or the specified entity tag may not match the part's entity tag.",
    )
}

fn complete(buckets: &mut Buckets, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let key = input.require_str("Key")?.to_string();
    let bucket_name = input.require_str("Bucket")?.to_string();
    let id = input.require_str("UploadId")?.to_string();
    let requested = input
        .obj_field("MultipartUpload")
        .and_then(|m| m.get("Parts"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if requested.is_empty() {
        return Err(err(
            400,
            "MalformedXML",
            "The XML you provided was not well-formed or did not validate against our published schema",
        ));
    }
    let upload = upload_mut(buckets, input)?;
    let mut chosen: Vec<&Part> = Vec::with_capacity(requested.len());
    let mut last = 0;
    for (i, p) in requested.iter().enumerate() {
        let n = p.i64_field("PartNumber").unwrap_or(0) as u32;
        if n <= last {
            return Err(err(
                400,
                "InvalidPartOrder",
                "The list of parts was not in ascending order. Parts must be ordered by part number.",
            ));
        }
        last = n;
        let part = upload.parts.get(&n).ok_or_else(invalid_part)?;
        if p.str_field("ETag")
            .is_some_and(|e| e.trim_matches('"') != part.etag.trim_matches('"'))
        {
            return Err(invalid_part());
        }
        if let Some(alg) = upload.algorithm
            && p.str_field(alg.member())
                .is_some_and(|c| Some(c) != part.checksum.as_deref())
        {
            return Err(invalid_part());
        }
        if i + 1 < requested.len() && part.body.len() < MIN_PART_SIZE {
            return Err(with(
                with(
                    err(
                        400,
                        "EntityTooSmall",
                        "Your proposed upload is smaller than the minimum allowed object size.",
                    ),
                    "ProposedSize",
                    part.body.len().to_string(),
                ),
                "MinSizeAllowed",
                MIN_PART_SIZE.to_string(),
            ));
        }
        chosen.push(part);
    }
    let etags: Vec<&str> = chosen.iter().map(|p| p.etag.as_str()).collect();
    let etag = multipart_etag(&etags);
    let data: Vec<u8> = chosen.iter().flat_map(|p| p.body.iter().copied()).collect();
    let checksum = match upload.algorithm {
        Some(alg) if !upload.full_object => {
            let sums: Vec<&str> = chosen
                .iter()
                .filter_map(|p| p.checksum.as_deref())
                .collect();
            Checksum {
                algorithm: alg,
                value: composite(alg, &sums),
                full_object: false,
            }
        }
        Some(alg) => Checksum::of(alg, &data),
        None => Checksum::of(Algorithm::Crc64Nvme, &data),
    };
    let parts = chosen
        .iter()
        .map(|p| PartInfo {
            size: p.body.len(),
            checksum: p.checksum.clone(),
        })
        .collect();
    let bucket = bucket_mut(buckets, input)?;
    let upload = bucket
        .uploads
        .remove(&id)
        .ok_or_else(|| no_such_upload(&id))?;
    let obj = Object {
        version_id: None,
        delete_marker: false,
        body: data,
        etag: etag.clone(),
        last_modified: now_secs(ctx),
        content_type: upload.content_type,
        attrs: upload.attrs,
        metadata: upload.metadata,
        storage_class: upload.storage_class,
        tags: upload.tags,
        acl: upload.acl,
        checksum: Some(checksum.clone()),
        parts,
    };
    let mut out = Map::new();
    out.insert(
        "Location".into(),
        json!(format!(
            "{}/{bucket_name}/{}",
            ctx.endpoint,
            crate::codec::rest::uri_encode(&key, true)
        )),
    );
    out.insert("Bucket".into(), json!(bucket_name));
    out.insert("Key".into(), json!(key));
    out.insert("ETag".into(), json!(etag));
    out.insert(checksum.algorithm.member().into(), json!(checksum.value));
    out.insert("ChecksumType".into(), json!(checksum.kind()));
    put(
        &mut out,
        "ServerSideEncryption",
        obj.attrs.get("ServerSideEncryption").cloned(),
    );
    put(&mut out, "VersionId", bucket.insert(ctx, &key, obj));
    Ok(Value::Object(out))
}

pub fn call(buckets: &mut Buckets, ctx: &mut Ctx<'_>, operation: &str, input: &Value) -> OpResult {
    match operation {
        "CreateMultipartUpload" => {
            let key = input.require_str("Key")?.to_string();
            let algorithm = match input.str_field("ChecksumAlgorithm") {
                Some(a) => Some(Algorithm::parse(a).ok_or_else(|| {
                    super::invalid_argument(
                        "Checksum algorithm is not supported",
                        "x-amz-checksum-algorithm",
                        a,
                    )
                })?),
                None => None,
            };
            let full_object = match input.str_field("ChecksumType") {
                Some("FULL_OBJECT") => {
                    if matches!(
                        algorithm,
                        Some(Algorithm::Sha1 | Algorithm::Sha256 | Algorithm::Sha512)
                    ) {
                        return Err(err(
                            400,
                            "InvalidRequest",
                            "The FULL_OBJECT checksum type cannot be used with the SHA algorithms.",
                        ));
                    }
                    true
                }
                Some("COMPOSITE") => {
                    if algorithm == Some(Algorithm::Crc64Nvme) {
                        return Err(err(
                            400,
                            "InvalidRequest",
                            "The COMPOSITE checksum type cannot be used with the crc64nvme checksum algorithm.",
                        ));
                    }
                    false
                }
                _ => algorithm.is_none_or(Algorithm::full_object_by_default),
            };
            let upload = Upload {
                key: key.clone(),
                initiated: now_secs(ctx),
                content_type: input
                    .str_field("ContentType")
                    .unwrap_or("binary/octet-stream")
                    .to_string(),
                attrs: attrs_from(input),
                metadata: input.obj_field("Metadata").cloned().unwrap_or_default(),
                storage_class: input
                    .str_field("StorageClass")
                    .unwrap_or("STANDARD")
                    .to_string(),
                tags: tags_from_header(input.str_field("Tagging")),
                acl: acl_from(ctx, input)?,
                algorithm,
                full_object,
                parts: Default::default(),
            };
            let id = format!("{}{}", super::version_id(ctx), super::version_id(ctx));
            let bucket_name = input.require_str("Bucket")?.to_string();
            let bucket = bucket_mut(buckets, input)?;
            let mut out = Map::new();
            out.insert("Bucket".into(), json!(bucket_name));
            out.insert("Key".into(), json!(key));
            out.insert("UploadId".into(), json!(id));
            put(
                &mut out,
                "ServerSideEncryption",
                upload.attrs.get("ServerSideEncryption").cloned(),
            );
            if let Some(a) = algorithm {
                out.insert("ChecksumAlgorithm".into(), json!(a.name()));
                out.insert(
                    "ChecksumType".into(),
                    json!(if full_object {
                        "FULL_OBJECT"
                    } else {
                        "COMPOSITE"
                    }),
                );
            }
            bucket.uploads.insert(id, upload);
            Ok(Value::Object(out))
        }
        "UploadPart" => {
            let number = part_number(input)?;
            let data = body(input);
            let given = verify_upload(input, &data)?;
            let upload = upload_mut(buckets, input)?;
            let part = store_part(ctx, upload, number, data, given);
            let mut out = Map::new();
            out.insert("ETag".into(), json!(part.etag));
            put(
                &mut out,
                "ServerSideEncryption",
                upload.attrs.get("ServerSideEncryption").cloned(),
            );
            if let (Some(alg), Some(c)) = (upload.algorithm, part.checksum) {
                out.insert(alg.member().into(), json!(c));
            }
            Ok(Value::Object(out))
        }
        "UploadPartCopy" => {
            let number = part_number(input)?;
            let (src, source_version) = source_object(buckets, input)?;
            let data = match input.str_field("CopySourceRange") {
                Some(r) => {
                    let invalid = || {
                        super::invalid_argument(
                            "The x-amz-copy-source-range value must be of the form bytes=first-last where first and last are the zero-based offsets of the first and last bytes to copy",
                            "x-amz-copy-source-range",
                            r,
                        )
                    };
                    let (s, e) = parse_range(r, src.body.len())
                        .map_err(|()| {
                            err(
                                416,
                                "InvalidRange",
                                "The requested range is not satisfiable",
                            )
                        })?
                        .ok_or_else(invalid)?;
                    src.body[s..=e].to_vec()
                }
                None => src.body,
            };
            let upload = upload_mut(buckets, input)?;
            let part = store_part(ctx, upload, number, data, None);
            let mut result = Map::new();
            result.insert("ETag".into(), json!(part.etag));
            result.insert("LastModified".into(), json!(part.last_modified));
            if let (Some(alg), Some(c)) = (upload.algorithm, part.checksum) {
                result.insert(alg.member().into(), json!(c));
            }
            let mut out = Map::new();
            out.insert("CopyPartResult".into(), Value::Object(result));
            put(&mut out, "CopySourceVersionId", source_version);
            Ok(Value::Object(out))
        }
        "CompleteMultipartUpload" => complete(buckets, ctx, input),
        _ => {
            let id = input.require_str("UploadId")?.to_string();
            upload_mut(buckets, input)?;
            bucket_mut(buckets, input)?.uploads.remove(&id);
            Ok(json!({}))
        }
    }
}

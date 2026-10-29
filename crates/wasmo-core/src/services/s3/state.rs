use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::checksum::{Algorithm, Checksum};
use super::{access_denied, version_id};
use crate::codec::AwsError;
use crate::codec::time::value_epoch;
use crate::service::Ctx;

/// Header members stored with an object and echoed by GetObject/HeadObject.
pub const ATTRS: &[&str] = &[
    "CacheControl",
    "ContentDisposition",
    "ContentEncoding",
    "ContentLanguage",
    "Expires",
    "WebsiteRedirectLocation",
    "ServerSideEncryption",
    "SSEKMSKeyId",
    "BucketKeyEnabled",
    "ObjectLockMode",
    "ObjectLockRetainUntilDate",
    "ObjectLockLegalHoldStatus",
];

pub fn attrs_from(input: &Value) -> Map<String, Value> {
    let mut attrs: Map<String, Value> = ATTRS
        .iter()
        .filter_map(|k| {
            Some((
                k.to_string(),
                input.get(*k).filter(|v| !v.is_null())?.clone(),
            ))
        })
        .collect();
    attrs
        .entry("ServerSideEncryption")
        .or_insert_with(|| Value::String("AES256".into()));
    attrs
}

pub fn tags_from_header(header: Option<&str>) -> Vec<(String, String)> {
    header
        .map(crate::codec::rest::parse_query)
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
pub struct PartInfo {
    pub size: usize,
    pub checksum: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Object {
    pub version_id: Option<String>,
    pub delete_marker: bool,
    pub body: Vec<u8>,
    pub etag: String,
    pub last_modified: f64,
    pub content_type: String,
    pub attrs: Map<String, Value>,
    pub metadata: Map<String, Value>,
    pub storage_class: String,
    pub tags: Vec<(String, String)>,
    pub acl: Value,
    pub checksum: Option<Checksum>,
    pub parts: Vec<PartInfo>,
}

impl Object {
    pub fn delete_marker(last_modified: f64) -> Self {
        Self {
            version_id: None,
            delete_marker: true,
            body: Vec::new(),
            etag: String::new(),
            last_modified,
            content_type: String::new(),
            attrs: Map::new(),
            metadata: Map::new(),
            storage_class: "STANDARD".into(),
            tags: Vec::new(),
            acl: Value::Null,
            checksum: None,
            parts: Vec::new(),
        }
    }

    pub fn version_label(&self) -> &str {
        self.version_id.as_deref().unwrap_or("null")
    }

    fn locked(&self, now: f64, bypass_governance: bool) -> bool {
        if self
            .attrs
            .get("ObjectLockLegalHoldStatus")
            .and_then(Value::as_str)
            == Some("ON")
        {
            return true;
        }
        let until = self
            .attrs
            .get("ObjectLockRetainUntilDate")
            .and_then(value_epoch);
        match (
            self.attrs.get("ObjectLockMode").and_then(Value::as_str),
            until,
        ) {
            (Some("COMPLIANCE"), Some(u)) => u > now,
            (Some("GOVERNANCE"), Some(u)) => u > now && !bypass_governance,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Part {
    pub body: Vec<u8>,
    pub etag: String,
    pub last_modified: f64,
    pub checksum: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Upload {
    pub key: String,
    pub initiated: f64,
    pub content_type: String,
    pub attrs: Map<String, Value>,
    pub metadata: Map<String, Value>,
    pub storage_class: String,
    pub tags: Vec<(String, String)>,
    pub acl: Value,
    pub algorithm: Option<Algorithm>,
    pub full_object: bool,
    pub parts: BTreeMap<u32, Part>,
}

#[derive(Debug, Clone)]
pub struct Bucket {
    pub region: String,
    pub created: f64,
    pub objects: BTreeMap<String, Vec<Object>>,
    pub uploads: BTreeMap<String, Upload>,
    pub versioning: Option<String>,
    pub mfa_delete: Option<String>,
    pub acl: Value,
    pub configs: BTreeMap<&'static str, Value>,
    pub keyed_configs: BTreeMap<(&'static str, String), Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deleted {
    pub version_id: Option<String>,
    pub delete_marker: bool,
}

pub fn matches_version(requested: &str, stored: &Option<String>) -> bool {
    match stored {
        Some(v) => v == requested,
        None => requested == "null",
    }
}

impl Bucket {
    pub fn new(region: String, created: f64) -> Self {
        Self {
            region,
            created,
            objects: BTreeMap::new(),
            uploads: BTreeMap::new(),
            versioning: None,
            mfa_delete: None,
            acl: Value::Null,
            configs: BTreeMap::new(),
            keyed_configs: BTreeMap::new(),
        }
    }

    pub fn current(&self, key: &str) -> Option<&Object> {
        self.objects.get(key)?.first().filter(|o| !o.delete_marker)
    }

    pub fn find(&self, key: &str, version: Option<&str>) -> Option<&Object> {
        match version {
            Some(v) => self
                .objects
                .get(key)?
                .iter()
                .find(|o| matches_version(v, &o.version_id)),
            None => self.objects.get(key)?.first(),
        }
    }

    pub fn find_mut(&mut self, key: &str, version: Option<&str>) -> Option<&mut Object> {
        let versions = self.objects.get_mut(key)?;
        match version {
            Some(v) => versions
                .iter_mut()
                .find(|o| matches_version(v, &o.version_id)),
            None => versions.first_mut().filter(|o| !o.delete_marker),
        }
    }

    /// Stores a new current version of `key` and returns its version id.
    pub fn insert(&mut self, ctx: &mut Ctx<'_>, key: &str, mut obj: Object) -> Option<String> {
        let versions = self.objects.entry(key.to_string()).or_default();
        match self.versioning.as_deref() {
            Some("Enabled") => {
                obj.version_id = Some(version_id(ctx));
            }
            Some(_) => {
                obj.version_id = None;
                versions.retain(|v| v.version_id.is_some());
            }
            None => {
                obj.version_id = None;
                versions.clear();
            }
        }
        let id = obj.version_id.clone();
        versions.insert(0, obj);
        id
    }

    pub fn delete(
        &mut self,
        ctx: &mut Ctx<'_>,
        key: &str,
        version: Option<&str>,
        bypass_governance: bool,
    ) -> Result<Deleted, AwsError> {
        let now = super::now_secs(ctx);
        if let Some(v) = version {
            let Some(versions) = self.objects.get_mut(key) else {
                return Ok(Deleted {
                    version_id: Some(v.to_string()),
                    delete_marker: false,
                });
            };
            let Some(i) = versions
                .iter()
                .position(|o| matches_version(v, &o.version_id))
            else {
                return Ok(Deleted {
                    version_id: Some(v.to_string()),
                    delete_marker: false,
                });
            };
            if versions[i].locked(now, bypass_governance) {
                return Err(access_denied());
            }
            let removed = versions.remove(i);
            if versions.is_empty() {
                self.objects.remove(key);
            }
            return Ok(Deleted {
                version_id: Some(v.to_string()),
                delete_marker: removed.delete_marker,
            });
        }
        if self.versioning.is_none() {
            self.objects.remove(key);
            return Ok(Deleted {
                version_id: None,
                delete_marker: false,
            });
        }
        let id = self.insert(ctx, key, Object::delete_marker(now));
        Ok(Deleted {
            version_id: Some(id.unwrap_or_else(|| "null".into())),
            delete_marker: true,
        })
    }
}

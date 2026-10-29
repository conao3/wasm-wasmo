use std::collections::HashMap;

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(untagged)]
pub enum XmlNamespace {
    #[default]
    None,
    Uri(String),
    Full {
        uri: Option<String>,
        prefix: Option<String>,
    },
}

impl XmlNamespace {
    pub fn attribute(&self) -> Option<(String, String)> {
        match self {
            XmlNamespace::None => None,
            XmlNamespace::Uri(uri) => Some(("xmlns".to_string(), uri.clone())),
            XmlNamespace::Full { uri, prefix } => {
                let name = prefix
                    .as_ref()
                    .map_or_else(|| "xmlns".to_string(), |p| format!("xmlns:{p}"));
                Some((name, uri.clone().unwrap_or_default()))
            }
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberRef {
    pub shape: String,
    pub location: Option<String>,
    pub location_name: Option<String>,
    #[serde(default)]
    pub flattened: bool,
    pub query_name: Option<String>,
    #[serde(default)]
    pub xml_namespace: XmlNamespace,
    #[serde(default)]
    pub xml_attribute: bool,
    pub timestamp_format: Option<String>,
    #[serde(default)]
    pub jsonvalue: bool,
    #[serde(default)]
    pub host_label: bool,
    #[serde(default)]
    pub idempotency_token: bool,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub event_payload: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorTrait {
    pub code: Option<String>,
    pub http_status_code: Option<u16>,
    #[serde(default)]
    pub sender_fault: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Shape {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub members: IndexMap<String, MemberRef>,
    pub member: Option<Box<MemberRef>>,
    pub key: Option<Box<MemberRef>>,
    pub value: Option<Box<MemberRef>>,
    #[serde(default)]
    pub flattened: bool,
    pub location_name: Option<String>,
    pub timestamp_format: Option<String>,
    pub payload: Option<String>,
    #[serde(default)]
    pub xml_namespace: XmlNamespace,
    #[serde(default)]
    pub required: Vec<String>,
    pub error: Option<ErrorTrait>,
    #[serde(default)]
    pub exception: bool,
    #[serde(default)]
    pub union: bool,
    #[serde(default)]
    pub document: bool,
    #[serde(default)]
    pub eventstream: bool,
    #[serde(rename = "enum")]
    pub enum_values: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Http {
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default = "default_uri")]
    pub request_uri: String,
    pub response_code: Option<u16>,
}

fn default_method() -> String {
    "POST".to_string()
}

fn default_uri() -> String {
    "/".to_string()
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputRef {
    pub shape: String,
    pub location_name: Option<String>,
    #[serde(default)]
    pub xml_namespace: XmlNamespace,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputRef {
    pub shape: String,
    pub result_wrapper: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Operation {
    pub name: String,
    #[serde(default)]
    pub http: Http,
    pub input: Option<InputRef>,
    pub output: Option<OutputRef>,
    #[serde(default)]
    pub errors: Vec<MemberRef>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Metadata {
    pub protocol: String,
    pub api_version: Option<String>,
    pub target_prefix: Option<String>,
    pub json_version: Option<String>,
    pub xml_namespace: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Registry {
    shapes: HashMap<String, Shape>,
}

impl Registry {
    pub fn from_value(shapes: &Value) -> Result<Self, serde_json::Error> {
        let shapes: HashMap<String, Shape> = serde_json::from_value(shapes.clone())?;
        Ok(Self { shapes })
    }

    pub fn shape(&self, name: &str) -> &Shape {
        self.shapes
            .get(name)
            .unwrap_or_else(|| panic!("wasmo: unknown shape {name}"))
    }

    pub fn member_shape(&self, member: &MemberRef) -> &Shape {
        self.shape(&member.shape)
    }
}

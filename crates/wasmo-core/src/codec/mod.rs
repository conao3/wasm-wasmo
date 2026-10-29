pub mod cbor;
pub mod json;
pub mod query;
pub mod rest;
pub mod scalar;
pub mod time;
pub mod xml_value;

#[derive(Debug, Clone)]
pub struct AwsError {
    pub code: String,
    pub message: String,
    pub status: u16,
    pub sender_fault: bool,
    pub data: Box<serde_json::Map<String, serde_json::Value>>,
}

impl AwsError {
    pub fn new(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            status,
            sender_fault: status < 500,
            data: Default::default(),
        }
    }
}

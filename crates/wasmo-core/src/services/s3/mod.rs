use serde_json::Value;

use crate::codec::AwsError;
use crate::embedded_model;
use crate::service::{Ctx, OpResult, Service, ServiceModel};

#[derive(Default)]
pub struct S3;

impl Service for S3 {
    fn signing_name(&self) -> &'static str {
        "s3"
    }

    fn model(&self) -> &'static ServiceModel {
        embedded_model!("s3")
    }

    fn call(&mut self, _ctx: &mut Ctx<'_>, operation: &str, _input: Value) -> OpResult {
        Err(AwsError::new(
            501,
            "NotImplemented",
            format!("wasmo: {operation} is not implemented"),
        ))
    }
}

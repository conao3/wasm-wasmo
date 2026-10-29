use serde_json::Value;

use crate::codec::AwsError;
use crate::embedded_model;
use crate::service::{Ctx, OpResult, Service, ServiceModel};

#[derive(Default)]
pub struct DynamoDb;

impl Service for DynamoDb {
    fn signing_name(&self) -> &'static str {
        "dynamodb"
    }

    fn model(&self) -> &'static ServiceModel {
        embedded_model!("dynamodb")
    }

    fn call(&mut self, _ctx: &mut Ctx<'_>, operation: &str, _input: Value) -> OpResult {
        Err(AwsError::new(
            400,
            "UnknownOperationException",
            format!("wasmo: {operation} is not implemented"),
        ))
    }
}

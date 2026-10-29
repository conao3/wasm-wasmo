use crate::service::Service;

pub mod dynamodb;
pub mod events;
pub mod kms;
mod pattern;
pub mod s3;
pub mod secretsmanager;
pub mod sns;
pub mod sqs;
pub mod ssm;
pub mod sts;

pub fn all() -> Vec<Box<dyn Service>> {
    let dynamo = dynamodb::Regions::default();
    vec![
        Box::new(sts::Sts),
        Box::new(sqs::Sqs::default()),
        Box::new(dynamodb::DynamoDb::new(dynamo.clone())),
        Box::new(dynamodb::DynamoDbStreams::new(dynamo)),
        Box::new(s3::S3::default()),
        Box::new(sns::Sns::default()),
        Box::new(events::Events::default()),
        Box::new(secretsmanager::SecretsManager::default()),
        Box::new(ssm::Ssm::default()),
        Box::new(kms::Kms::default()),
    ]
}

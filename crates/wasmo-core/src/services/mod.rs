use crate::service::Service;

pub mod dynamodb;
mod pattern;
pub mod s3;
pub mod secretsmanager;
pub mod sns;
pub mod sqs;
pub mod ssm;
pub mod sts;

pub fn all() -> Vec<Box<dyn Service>> {
    vec![
        Box::new(sts::Sts),
        Box::new(sqs::Sqs::default()),
        Box::new(dynamodb::DynamoDb::default()),
        Box::new(s3::S3::default()),
        Box::new(sns::Sns::default()),
        Box::new(secretsmanager::SecretsManager::default()),
        Box::new(ssm::Ssm::default()),
    ]
}

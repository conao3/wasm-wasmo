use crate::service::Service;

pub mod dynamodb;
pub mod s3;
pub mod sqs;
pub mod sts;

pub fn all() -> Vec<Box<dyn Service>> {
    vec![
        Box::new(sts::Sts),
        Box::new(sqs::Sqs::default()),
        Box::new(dynamodb::DynamoDb),
        Box::new(s3::S3::default()),
    ]
}

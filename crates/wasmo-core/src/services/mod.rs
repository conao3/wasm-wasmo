use crate::service::Service;

pub mod cloudwatch;
pub mod dynamodb;
pub mod events;
pub mod firehose;
pub mod iam;
pub mod kinesis;
pub mod kms;
pub mod logs;
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
        Box::new(kinesis::Kinesis::default()),
        Box::new(firehose::Firehose::default()),
        Box::new(logs::Logs::default()),
        Box::new(cloudwatch::CloudWatch::default()),
        Box::new(iam::Iam::default()),
    ]
}

pub(crate) fn gzip(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).expect("writes to a vec");
    e.finish().expect("writes to a vec")
}

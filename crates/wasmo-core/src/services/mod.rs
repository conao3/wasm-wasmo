use crate::service::Service;

pub mod sqs;
pub mod sts;

pub fn all() -> Vec<Box<dyn Service>> {
    vec![Box::new(sts::Sts), Box::new(sqs::Sqs::default())]
}

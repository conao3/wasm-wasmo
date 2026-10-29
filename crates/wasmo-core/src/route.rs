use crate::codec::rest::parse_query;
use crate::wire::Request;

pub const DEFAULT_ACCOUNT: &str = "000000000000";
pub const DEFAULT_REGION: &str = "us-east-1";

const ENDPOINT_HOSTS: &[&str] = &["wasmo.local", "localhost"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub service: Option<String>,
    pub region: String,
    pub account: String,
    pub target: Option<String>,
}

impl Route {
    pub fn from_request(req: &Request) -> Self {
        let scope = req
            .header("authorization")
            .and_then(|a| a.split_once("Credential=").map(|(_, c)| c))
            .and_then(|c| credential_scope(c.split([',', ' ']).next()?))
            .or_else(|| {
                parse_query(&req.query)
                    .into_iter()
                    .find(|(k, _)| k == "X-Amz-Credential")
                    .and_then(|(_, c)| credential_scope(&c))
            });
        let target = req.header("x-amz-target").map(str::to_string);
        let service = scope.as_ref().map(|(_, service)| service.clone());
        Self {
            service,
            region: scope.map_or_else(|| DEFAULT_REGION.to_string(), |(region, _)| region),
            account: DEFAULT_ACCOUNT.to_string(),
            target,
        }
    }
}

fn credential_scope(credential: &str) -> Option<(String, String)> {
    let mut parts = credential.split('/');
    let _key = parts.next()?;
    let _date = parts.next()?;
    let region = parts.next()?;
    let service = parts.next()?;
    Some((region.to_string(), service.to_string()))
}

/// Splits a virtual-hosted-style S3 host (`<bucket>.wasmo.local`,
/// `<bucket>.s3.<region>.amazonaws.com`) into the bucket and the endpoint host.
pub fn virtual_host_bucket(host: &str) -> Option<(&str, &str)> {
    let hostname = match host.rsplit_once(':') {
        Some((h, port)) if port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => host,
    };
    if let Some(base) = ENDPOINT_HOSTS.iter().find(|base| {
        hostname.len() > base.len() + 1
            && hostname.ends_with(*base)
            && hostname.as_bytes()[hostname.len() - base.len() - 1] == b'.'
    }) {
        let bucket = &hostname[..hostname.len() - base.len() - 1];
        return Some((bucket, &host[bucket.len() + 1..]));
    }
    if hostname.ends_with(".amazonaws.com") {
        let at = [".s3.", ".s3-"]
            .iter()
            .filter_map(|m| hostname.find(m))
            .min()?;
        return (at > 0).then(|| (&hostname[..at], &host[at + 1..]));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{Request, RequestMeta};

    fn req(url: &str, headers: &[(&str, &str)]) -> Request {
        Request::from_meta(
            RequestMeta {
                method: "POST".into(),
                url: url.into(),
                headers: headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            },
            vec![],
        )
    }

    #[test]
    fn service_and_region_from_credential_scope() {
        let r = Route::from_request(&req(
            "http://wasmo.local/",
            &[(
                "Authorization",
                "AWS4-HMAC-SHA256 Credential=test/20260101/eu-west-1/sqs/aws4_request, SignedHeaders=host, Signature=x",
            )],
        ));
        assert_eq!(r.service.as_deref(), Some("sqs"));
        assert_eq!(r.region, "eu-west-1");
    }

    #[test]
    fn service_and_region_from_presigned_query() {
        let r = Route::from_request(&req(
            "http://b.wasmo.local/k?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=test%2F20260101%2Fap-northeast-1%2Fs3%2Faws4_request&X-Amz-Signature=x",
            &[],
        ));
        assert_eq!(r.service.as_deref(), Some("s3"));
        assert_eq!(r.region, "ap-northeast-1");
    }

    #[test]
    fn virtual_hosts() {
        assert_eq!(
            virtual_host_bucket("photos.wasmo.local"),
            Some(("photos", "wasmo.local"))
        );
        assert_eq!(
            virtual_host_bucket("my.dotted.bucket.localhost:4566"),
            Some(("my.dotted.bucket", "localhost:4566"))
        );
        assert_eq!(
            virtual_host_bucket("logs.s3.us-west-2.amazonaws.com"),
            Some(("logs", "s3.us-west-2.amazonaws.com"))
        );
        assert_eq!(virtual_host_bucket("wasmo.local"), None);
        assert_eq!(virtual_host_bucket("xwasmo.local"), None);
        assert_eq!(virtual_host_bucket("s3.amazonaws.com"), None);
        assert_eq!(virtual_host_bucket("127.0.0.1:9000"), None);
    }
}

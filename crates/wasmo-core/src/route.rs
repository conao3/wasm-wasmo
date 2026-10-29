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
            })
            .or_else(|| is_sigv2(req).then(|| (DEFAULT_REGION.to_string(), "s3".to_string())));
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

/// SigV2 (presigned `AWSAccessKeyId` + `Signature`, or `Authorization: AWS key:sig`)
/// is only used by S3.
fn is_sigv2(req: &Request) -> bool {
    req.header("authorization")
        .is_some_and(|a| a.starts_with("AWS "))
        || parse_query(&req.query)
            .iter()
            .any(|(k, _)| k == "AWSAccessKeyId")
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

/// An `execute-api` request: the API id, the region from the host, and the
/// path starting with the stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecuteApi {
    pub id: String,
    pub region: String,
    pub path: String,
}

/// Recognizes `{api}.execute-api.{region}.amazonaws.com`, the same host under
/// `wasmo.local` or `localhost` (`{api}.execute-api.wasmo.local`,
/// `{api}.execute-api.{region}.localhost`), and the path form
/// `/restapis/{api}/{stage}/_user_request_/{path}` on any host.
pub fn execute_api(host: &str, path: &str) -> Option<ExecuteApi> {
    let hostname = match host.rsplit_once(':') {
        Some((h, port)) if port.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => host,
    };
    if let Some((id, rest)) = hostname.split_once(".execute-api.")
        && !id.is_empty()
        && !id.contains('.')
    {
        let label = rest.split('.').next().unwrap_or_default();
        let region = if is_region(label) {
            label
        } else {
            DEFAULT_REGION
        };
        return Some(ExecuteApi {
            id: id.to_string(),
            region: region.to_string(),
            path: path.to_string(),
        });
    }
    let rest = path.strip_prefix("/restapis/")?;
    let (id, rest) = rest.split_once('/')?;
    let (stage, rest) = rest.split_once("/_user_request_")?;
    Some(ExecuteApi {
        id: id.to_string(),
        region: DEFAULT_REGION.to_string(),
        path: format!("/{stage}{}", if rest.is_empty() { "/" } else { rest }),
    })
}

fn is_region(label: &str) -> bool {
    label.split('-').count() >= 3 && label.ends_with(|c: char| c.is_ascii_digit())
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
    fn execute_api_hosts() {
        let api = execute_api("abc123.execute-api.eu-west-1.amazonaws.com", "/prod/pets").unwrap();
        assert_eq!(
            (api.id.as_str(), api.region.as_str(), api.path.as_str()),
            ("abc123", "eu-west-1", "/prod/pets")
        );
        let api = execute_api("abc123.execute-api.wasmo.local", "/pets").unwrap();
        assert_eq!(
            (api.id.as_str(), api.region.as_str()),
            ("abc123", "us-east-1")
        );
        let api = execute_api("wasmo.local", "/restapis/abc123/dev/_user_request_/a/b").unwrap();
        assert_eq!((api.id.as_str(), api.path.as_str()), ("abc123", "/dev/a/b"));
        assert_eq!(
            execute_api("wasmo.local", "/restapis/abc123/resources"),
            None
        );
        assert_eq!(execute_api("sqs.us-east-1.amazonaws.com", "/"), None);
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

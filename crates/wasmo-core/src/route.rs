use crate::wire::Request;

pub const DEFAULT_ACCOUNT: &str = "000000000000";
pub const DEFAULT_REGION: &str = "us-east-1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub service: Option<String>,
    pub region: String,
    pub account: String,
    pub target: Option<String>,
}

impl Route {
    pub fn from_request(req: &Request) -> Self {
        let scope = req.header("authorization").and_then(credential_scope);
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

fn credential_scope(authorization: &str) -> Option<(String, String)> {
    let credential = authorization.split_once("Credential=")?.1;
    let credential = credential.split([',', ' ']).next()?;
    let mut parts = credential.split('/');
    let _key = parts.next()?;
    let _date = parts.next()?;
    let region = parts.next()?;
    let service = parts.next()?;
    Some((region.to_string(), service.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{Request, RequestMeta};

    fn req(headers: &[(&str, &str)]) -> Request {
        Request::from_meta(
            RequestMeta {
                method: "POST".into(),
                url: "http://wasmo.local/".into(),
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
        let r = Route::from_request(&req(&[(
            "Authorization",
            "AWS4-HMAC-SHA256 Credential=test/20260101/eu-west-1/sqs/aws4_request, SignedHeaders=host, Signature=x",
        )]));
        assert_eq!(r.service.as_deref(), Some("sqs"));
        assert_eq!(r.region, "eu-west-1");
    }
}

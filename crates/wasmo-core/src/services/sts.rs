use serde_json::{Value, json};

use crate::codec::AwsError;
use crate::embedded_model;
use crate::service::{Ctx, InputExt, OpResult, Service, ServiceModel};

const ROLE_TOKEN_PREFIX: &str = "FQoGZXIvYXdzEXAMPLEtokenwasmo";
const FED_TOKEN_PREFIX: &str = "FQoGZXIvYXdzEXAMPLEtokenfedwasmo";
const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

pub struct Sts;

struct Role<'a> {
    name: &'a str,
    session: &'a str,
}

fn account_from_arn<'a>(arn: &'a str, fallback: &'a str) -> &'a str {
    arn.split(':')
        .nth(4)
        .filter(|a| a.len() == 12 && a.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or(fallback)
}

fn credentials(ctx: &Ctx<'_>, account: &str, duration: i64, role: Option<&Role<'_>>) -> Value {
    let token = match role {
        Some(r) => format!("{ROLE_TOKEN_PREFIX}{account}/{}/{}", r.name, r.session),
        None => format!("{ROLE_TOKEN_PREFIX}{account}"),
    };
    json!({
        "AccessKeyId": format!("ASIA{account}WSMO"),
        "SecretAccessKey": SECRET,
        "SessionToken": token,
        "Expiration": (ctx.now_ms() / 1000.0).floor() as i64 + duration,
    })
}

fn assumed(account: &str, role: &Role<'_>) -> Value {
    json!({
        "AssumedRoleId": format!("AROAWASMOEXAMPLEID:{}", role.session),
        "Arn": format!("arn:aws:sts::{account}:assumed-role/{}/{}", role.name, role.session),
    })
}

fn validation(message: &str) -> AwsError {
    AwsError::new(400, "ValidationError", message)
}

impl Service for Sts {
    fn signing_name(&self) -> &'static str {
        "sts"
    }

    fn model(&self) -> &'static ServiceModel {
        embedded_model!("sts")
    }

    fn call(&mut self, ctx: &mut Ctx<'_>, operation: &str, input: Value) -> OpResult {
        let account = ctx.account.clone();
        let duration = |default: i64| input.i64_field("DurationSeconds").unwrap_or(default);
        let default_role = format!("arn:aws:iam::{account}:role/wasmo");
        Ok(match operation {
            "GetCallerIdentity" => {
                let token = ctx.security_token.as_deref().unwrap_or_default();
                if let Some(rest) = token.strip_prefix(ROLE_TOKEN_PREFIX)
                    && let [acct, role, session] = rest.splitn(3, '/').collect::<Vec<_>>()[..]
                {
                    json!({
                        "Account": acct,
                        "Arn": format!("arn:aws:sts::{acct}:assumed-role/{role}/{session}"),
                        "UserId": format!("AROAWASMOEXAMPLEID:{session}"),
                    })
                } else if let Some(rest) = token.strip_prefix(FED_TOKEN_PREFIX)
                    && let Some((acct, name)) = rest.split_once('/')
                {
                    json!({
                        "Account": acct,
                        "Arn": format!("arn:aws:sts::{acct}:federated-user/{name}"),
                        "UserId": format!("{acct}:{name}"),
                    })
                } else {
                    json!({ "Account": account, "Arn": format!("arn:aws:iam::{account}:root"), "UserId": account })
                }
            }
            "AssumeRole" | "AssumeRoleWithWebIdentity" | "AssumeRoleWithSAML" => {
                if operation == "AssumeRoleWithWebIdentity"
                    && input.str_field("WebIdentityToken").is_none()
                {
                    return Err(AwsError::new(
                        400,
                        "InvalidIdentityToken",
                        "WebIdentityToken is required",
                    ));
                }
                if operation == "AssumeRoleWithSAML" && input.str_field("SAMLAssertion").is_none() {
                    return Err(AwsError::new(
                        400,
                        "InvalidIdentityToken",
                        "SAMLAssertion is required",
                    ));
                }
                let role_arn = input.str_field("RoleArn").unwrap_or(&default_role);
                let session = match operation {
                    "AssumeRoleWithSAML" => "wasmo-saml-session",
                    _ => input
                        .str_field("RoleSessionName")
                        .unwrap_or("wasmo-session"),
                };
                let role = Role {
                    name: role_arn.rsplit('/').next().unwrap_or("wasmo"),
                    session,
                };
                let acct = account_from_arn(role_arn, &account);
                let mut out = json!({
                    "Credentials": credentials(ctx, acct, duration(3600), Some(&role)),
                    "AssumedRoleUser": assumed(acct, &role),
                    "PackedPolicySize": 6,
                });
                match operation {
                    "AssumeRoleWithWebIdentity" => {
                        out["SubjectFromWebIdentityToken"] = json!("wasmo-web-identity-subject");
                        out["Provider"] =
                            json!(input.str_field("ProviderId").unwrap_or("sts.amazonaws.com"));
                        out["Audience"] = json!("wasmo");
                    }
                    "AssumeRoleWithSAML" => {
                        out["Subject"] = json!("wasmo-saml-subject");
                        out["SubjectType"] = json!("transient");
                        out["Issuer"] = json!("https://wasmo.example.com/saml");
                        out["Audience"] = json!("https://signin.aws.amazon.com/saml");
                        out["NameQualifier"] = json!("wasmo-name-qualifier");
                    }
                    _ => {}
                }
                out
            }
            "GetSessionToken" => {
                json!({ "Credentials": credentials(ctx, &account, duration(43200), None) })
            }
            "GetFederationToken" => {
                let name = input.str_field("Name").unwrap_or("wasmo-federated");
                let mut creds = credentials(ctx, &account, duration(43200), None);
                creds["SessionToken"] = json!(format!("{FED_TOKEN_PREFIX}{account}/{name}"));
                json!({
                    "Credentials": creds,
                    "FederatedUser": {
                        "FederatedUserId": format!("{account}:{name}"),
                        "Arn": format!("arn:aws:sts::{account}:federated-user/{name}"),
                    },
                    "PackedPolicySize": 6,
                })
            }
            "AssumeRoot" => {
                if input.str_field("TargetPrincipal").is_none() {
                    return Err(validation("TargetPrincipal is required"));
                }
                if input.get("TaskPolicyArn").is_none() {
                    return Err(validation("TaskPolicyArn is required"));
                }
                let role = Role {
                    name: "root",
                    session: "wasmo-root-session",
                };
                json!({ "Credentials": credentials(ctx, &account, duration(900), Some(&role)), "SourceIdentity": "wasmo-root" })
            }
            "DecodeAuthorizationMessage" => {
                use base64::Engine;
                let encoded = input.str_field("EncodedMessage").unwrap_or_default();
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .ok()
                    .and_then(|b| String::from_utf8(b).ok())
                    .unwrap_or_else(|| encoded.to_string());
                json!({ "DecodedMessage": decoded })
            }
            "GetAccessKeyInfo" => {
                let key = input.str_field("AccessKeyId").unwrap_or_default();
                let acct = key
                    .get(4..16)
                    .filter(|a| {
                        (key.starts_with("ASIA") || key.starts_with("AKIA"))
                            && a.bytes().all(|b| b.is_ascii_digit())
                    })
                    .unwrap_or(&account);
                json!({ "Account": acct })
            }
            "GetDelegatedAccessToken" => {
                if input.str_field("TradeInToken").is_none() {
                    return Err(AwsError::new(
                        400,
                        "ExpiredTradeInTokenException",
                        "TradeInToken is required",
                    ));
                }
                json!({
                    "Credentials": credentials(ctx, &account, 3600, None),
                    "PackedPolicySize": 6,
                    "AssumedPrincipal": format!("arn:aws:iam::{account}:user/wasmo"),
                })
            }
            "GetWebIdentityToken" => {
                if input.array_field("Audience").is_empty() {
                    return Err(validation("Audience is required"));
                }
                if input.str_field("SigningAlgorithm").is_none() {
                    return Err(validation("SigningAlgorithm is required"));
                }
                json!({
                    "WebIdentityToken": "eyJhbGciOiJSUzI1NiJ9.wasmo-web-identity-payload.wasmo-signature",
                    "Expiration": (ctx.now_ms() / 1000.0).floor() as i64 + duration(300),
                })
            }
            other => {
                return Err(AwsError::new(
                    400,
                    "InvalidAction",
                    format!("Could not find operation {other} for version 2011-06-15"),
                ));
            }
        })
    }
}

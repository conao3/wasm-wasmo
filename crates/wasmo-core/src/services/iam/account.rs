use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use super::{
    AccessKey, Account, Handled, Tags, arn, conflict, exists, invalid, no_such, now, page_value,
    path_of, random_chars, required, strings, tag_op, tag_values, tags_of, unique_id,
};
use crate::codec::AwsError;
use crate::codec::time::format_iso8601;
use crate::service::{Ctx, InputExt};

const UPPER: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
const SECRET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const HEX: &[u8] = b"0123456789abcdef";

fn key_view(k: &AccessKey, secret: bool) -> Value {
    let mut v = json!({
        "UserName": k.user,
        "AccessKeyId": k.id,
        "Status": k.status,
        "CreateDate": k.created,
    });
    if secret {
        v["SecretAccessKey"] = json!(k.secret);
    }
    v
}

fn check_password(acct: &Account, password: &str) -> Result<(), AwsError> {
    let Some(p) = &acct.password_policy else {
        return Ok(());
    };
    let flag = |k: &str| p.get(k).and_then(Value::as_bool) == Some(true);
    let min = p
        .get("MinimumPasswordLength")
        .and_then(Value::as_i64)
        .unwrap_or(6) as usize;
    let violations = [
        (
            password.chars().count() < min,
            format!("Password should have a minimum length of {min}"),
        ),
        (
            flag("RequireUppercaseCharacters") && !password.chars().any(|c| c.is_ascii_uppercase()),
            "Password should have at least one uppercase letter".to_string(),
        ),
        (
            flag("RequireLowercaseCharacters") && !password.chars().any(|c| c.is_ascii_lowercase()),
            "Password should have at least one lowercase letter".to_string(),
        ),
        (
            flag("RequireNumbers") && !password.chars().any(|c| c.is_ascii_digit()),
            "Password should have at least one number".to_string(),
        ),
        (
            flag("RequireSymbols") && password.chars().all(char::is_alphanumeric),
            "Password should have at least one symbol".to_string(),
        ),
    ];
    match violations.into_iter().find(|(bad, _)| *bad) {
        Some((_, m)) => Err(AwsError::new(400, "PasswordPolicyViolation", m)),
        None => Ok(()),
    }
}

fn user_items<'a>(
    items: &'a mut [Value],
    user: &str,
    id_key: &str,
    id: &str,
) -> Result<&'a mut Value, AwsError> {
    items
        .iter_mut()
        .find(|v| v["UserName"] == user && v[id_key] == id)
        .ok_or_else(|| no_such(format!("The {id_key} with id {id} cannot be found.")))
}

fn status(input: &Value) -> Result<String, AwsError> {
    match required(input, "Status")? {
        s @ ("Active" | "Inactive") => Ok(s.to_string()),
        s => Err(AwsError::new(
            400,
            "ValidationError",
            format!(
                "1 validation error detected: Value '{s}' at 'status' failed to satisfy constraint: Member must satisfy enum value set: [Active, Inactive]"
            ),
        )),
    }
}

fn tags_mut<'a>(
    acct: &'a mut Account,
    kind: &str,
    input: &Value,
) -> Result<&'a mut Tags, AwsError> {
    let key = match kind {
        "MFADevice" => required(input, "SerialNumber")?,
        "OpenIDConnectProvider" => required(input, "OpenIDConnectProviderArn")?,
        "SAMLProvider" => required(input, "SAMLProviderArn")?,
        _ => required(input, "ServerCertificateName")?,
    }
    .to_string();
    let known = match kind {
        "MFADevice" => acct.virtual_mfa.contains_key(&key),
        "OpenIDConnectProvider" => acct.oidc.contains_key(&key),
        "SAMLProvider" => acct.saml.contains_key(&key),
        _ => acct.server_certs.contains_key(&key),
    };
    if !known {
        return Err(no_such(format!("The {kind} {key} cannot be found.")));
    }
    Ok(acct
        .resource_tags
        .entry(format!("{kind}/{key}"))
        .or_default())
}

pub fn credentials(
    acct: &mut Account,
    ctx: &mut Ctx<'_>,
    operation: &str,
    input: &Value,
) -> Handled {
    let t = now(ctx);
    Ok(Some(match operation {
        "CreateAccessKey" => {
            let user = required(input, "UserName")?.to_string();
            acct.user(&user)?;
            if acct.access_keys.iter().filter(|k| k.user == user).count() >= 2 {
                return Err(AwsError::new(
                    409,
                    "LimitExceeded",
                    "Cannot exceed quota for AccessKeysPerUser: 2",
                ));
            }
            let k = AccessKey {
                id: format!("AKIA{}", random_chars(ctx, 16, UPPER)),
                secret: random_chars(ctx, 40, SECRET),
                status: "Active".to_string(),
                created: t,
                user,
            };
            let out = json!({ "AccessKey": key_view(&k, true) });
            acct.access_keys.push(k);
            out
        }
        "ListAccessKeys" => {
            let user = required(input, "UserName")?;
            acct.user(user)?;
            let keys = acct
                .access_keys
                .iter()
                .filter(|k| k.user == user)
                .map(|k| key_view(k, false))
                .collect();
            page_value(keys, input, "AccessKeyMetadata")
        }
        "UpdateAccessKey" | "DeleteAccessKey" => {
            let id = required(input, "AccessKeyId")?;
            let i = acct
                .access_keys
                .iter()
                .position(|k| k.id == id && input.str_field("UserName").is_none_or(|u| u == k.user))
                .ok_or_else(|| no_such(format!("The Access Key with id {id} cannot be found.")))?;
            if operation == "DeleteAccessKey" {
                acct.access_keys.remove(i);
            } else {
                acct.access_keys[i].status = status(input)?;
            }
            json!({})
        }
        "GetAccessKeyLastUsed" => {
            let id = required(input, "AccessKeyId")?;
            let k = acct
                .access_keys
                .iter()
                .find(|k| k.id == id)
                .ok_or_else(|| no_such(format!("The Access Key with id {id} cannot be found.")))?;
            json!({ "UserName": k.user, "AccessKeyLastUsed": { "ServiceName": "N/A", "Region": "N/A" } })
        }
        "CreateLoginProfile" | "UpdateLoginProfile" => {
            let user = required(input, "UserName")?.to_string();
            let password = input.str_field("Password");
            if operation == "CreateLoginProfile" && password.is_none() {
                return Err(invalid("Password is required."));
            }
            if let Some(p) = password {
                check_password(acct, p)?;
            }
            let reset = input.get("PasswordResetRequired").and_then(Value::as_bool);
            let u = acct.user_mut(&user)?;
            match (operation, &mut u.login) {
                ("CreateLoginProfile", Some(_)) => {
                    return Err(exists(format!(
                        "Login Profile for user {user} already exists."
                    )));
                }
                ("CreateLoginProfile", login) => *login = Some((t, reset.unwrap_or(false))),
                (_, None) => {
                    return Err(no_such(format!(
                        "Login Profile for User {user} cannot be found."
                    )));
                }
                (_, Some(l)) => {
                    if let Some(r) = reset {
                        l.1 = r;
                    }
                }
            }
            if operation == "CreateLoginProfile" {
                let (at, r) = u.login.expect("login profile was set");
                json!({ "LoginProfile": { "UserName": user, "CreateDate": at, "PasswordResetRequired": r } })
            } else {
                json!({})
            }
        }
        "GetLoginProfile" | "DeleteLoginProfile" => {
            let user = required(input, "UserName")?.to_string();
            let u = acct.user_mut(&user)?;
            let (at, r) = u.login.ok_or_else(|| {
                no_such(format!("Login Profile for User {user} cannot be found."))
            })?;
            if operation == "DeleteLoginProfile" {
                u.login = None;
                json!({})
            } else {
                json!({ "LoginProfile": { "UserName": user, "CreateDate": at, "PasswordResetRequired": r } })
            }
        }
        "ChangePassword" => {
            required(input, "OldPassword")?;
            check_password(acct, required(input, "NewPassword")?)?;
            json!({})
        }
        "UploadSSHPublicKey" => {
            let user = required(input, "UserName")?.to_string();
            acct.user(&user)?;
            let body = required(input, "SSHPublicKeyBody")?;
            if !body.starts_with("ssh-")
                && !body.starts_with("ecdsa-")
                && !body.starts_with("---- BEGIN")
            {
                return Err(AwsError::new(
                    400,
                    "UnrecognizedPublicKeyEncoding",
                    "Unrecognized public key encoding.",
                ));
            }
            if acct
                .ssh_keys
                .iter()
                .any(|k| k["UserName"] == user.as_str() && k["SSHPublicKeyBody"] == body)
            {
                return Err(AwsError::new(
                    400,
                    "DuplicateSSHPublicKey",
                    "The public key already exists.",
                ));
            }
            let fp: String = random_chars(ctx, 32, HEX)
                .as_bytes()
                .chunks(2)
                .map(|c| String::from_utf8_lossy(c).to_string())
                .collect::<Vec<_>>()
                .join(":");
            let k = json!({
                "UserName": user,
                "SSHPublicKeyId": format!("APKA{}", random_chars(ctx, 16, UPPER)),
                "Fingerprint": fp,
                "SSHPublicKeyBody": body,
                "Status": "Active",
                "UploadDate": t,
            });
            acct.ssh_keys.push(k.clone());
            json!({ "SSHPublicKey": k })
        }
        "GetSSHPublicKey" => {
            let k = user_items(
                &mut acct.ssh_keys,
                required(input, "UserName")?,
                "SSHPublicKeyId",
                required(input, "SSHPublicKeyId")?,
            )?;
            json!({ "SSHPublicKey": k })
        }
        "UpdateSSHPublicKey" => {
            let s = status(input)?;
            user_items(
                &mut acct.ssh_keys,
                required(input, "UserName")?,
                "SSHPublicKeyId",
                required(input, "SSHPublicKeyId")?,
            )?["Status"] = json!(s);
            json!({})
        }
        "DeleteSSHPublicKey" => {
            let (user, id) = (
                required(input, "UserName")?,
                required(input, "SSHPublicKeyId")?,
            );
            user_items(&mut acct.ssh_keys, user, "SSHPublicKeyId", id)?;
            acct.ssh_keys
                .retain(|k| !(k["UserName"] == user && k["SSHPublicKeyId"] == id));
            json!({})
        }
        "ListSSHPublicKeys" => {
            let user = input.str_field("UserName");
            let keys = acct.ssh_keys.iter().filter(|k| user.is_none_or(|u| k["UserName"] == u)).map(|k| json!({ "UserName": k["UserName"], "SSHPublicKeyId": k["SSHPublicKeyId"], "Status": k["Status"], "UploadDate": k["UploadDate"] })).collect();
            page_value(keys, input, "SSHPublicKeys")
        }
        "UploadSigningCertificate" => {
            let user = required(input, "UserName")?.to_string();
            acct.user(&user)?;
            let body = required(input, "CertificateBody")?;
            if !body.contains("BEGIN CERTIFICATE") {
                return Err(AwsError::new(
                    400,
                    "MalformedCertificate",
                    "Certificate body is not a PEM-encoded certificate.",
                ));
            }
            let c = json!({
                "UserName": user,
                "CertificateId": random_chars(ctx, 32, UPPER),
                "CertificateBody": body,
                "Status": "Active",
                "UploadDate": t,
            });
            acct.signing_certs.push(c.clone());
            json!({ "Certificate": c })
        }
        "UpdateSigningCertificate" => {
            let s = status(input)?;
            let user = input.str_field("UserName").unwrap_or_default();
            let id = required(input, "CertificateId")?;
            let c = acct
                .signing_certs
                .iter_mut()
                .find(|c| c["CertificateId"] == id && (user.is_empty() || c["UserName"] == user))
                .ok_or_else(|| no_such(format!("The Certificate with id {id} cannot be found.")))?;
            c["Status"] = json!(s);
            json!({})
        }
        "DeleteSigningCertificate" => {
            let id = required(input, "CertificateId")?;
            let before = acct.signing_certs.len();
            acct.signing_certs.retain(|c| c["CertificateId"] != id);
            if before == acct.signing_certs.len() {
                return Err(no_such(format!(
                    "The Certificate with id {id} cannot be found."
                )));
            }
            json!({})
        }
        "ListSigningCertificates" => {
            let user = input.str_field("UserName");
            let certs = acct
                .signing_certs
                .iter()
                .filter(|c| user.is_none_or(|u| c["UserName"] == u))
                .cloned()
                .collect();
            page_value(certs, input, "Certificates")
        }
        "CreateServiceSpecificCredential" => {
            let user = required(input, "UserName")?.to_string();
            acct.user(&user)?;
            let service = required(input, "ServiceName")?.to_string();
            if acct
                .service_creds
                .iter()
                .filter(|c| c["UserName"] == user.as_str() && c["ServiceName"] == service.as_str())
                .count()
                >= 2
            {
                return Err(AwsError::new(
                    409,
                    "LimitExceeded",
                    "Cannot exceed quota for ServiceSpecificCredentialsPerUserPerService: 2",
                ));
            }
            let c = json!({
                "UserName": user,
                "ServiceName": service,
                "ServiceUserName": format!("{user}-at-{}", ctx.account),
                "ServicePassword": random_chars(ctx, 44, SECRET),
                "ServiceSpecificCredentialId": format!("ACCA{}", random_chars(ctx, 17, UPPER)),
                "Status": "Active",
                "CreateDate": t,
            });
            acct.service_creds.push(c.clone());
            json!({ "ServiceSpecificCredential": c })
        }
        "ResetServiceSpecificCredential"
        | "UpdateServiceSpecificCredential"
        | "DeleteServiceSpecificCredential" => {
            let id = required(input, "ServiceSpecificCredentialId")?;
            let user = input.str_field("UserName");
            let i = acct
                .service_creds
                .iter()
                .position(|c| {
                    c["ServiceSpecificCredentialId"] == id
                        && user.is_none_or(|u| c["UserName"] == u)
                })
                .ok_or_else(|| no_such(format!("No such credential {id} exists.")))?;
            match operation {
                "DeleteServiceSpecificCredential" => {
                    acct.service_creds.remove(i);
                    json!({})
                }
                "UpdateServiceSpecificCredential" => {
                    acct.service_creds[i]["Status"] = json!(status(input)?);
                    json!({})
                }
                _ => {
                    acct.service_creds[i]["ServicePassword"] = json!(random_chars(ctx, 44, SECRET));
                    json!({ "ServiceSpecificCredential": acct.service_creds[i] })
                }
            }
        }
        "ListServiceSpecificCredentials" => {
            let user = input.str_field("UserName");
            let service = input.str_field("ServiceName");
            json!({ "ServiceSpecificCredentials": acct.service_creds.iter()
                .filter(|c| user.is_none_or(|u| c["UserName"] == u) && service.is_none_or(|s| c["ServiceName"] == s))
                .map(|c| {
                    let mut m = c.clone();
                    if let Some(o) = m.as_object_mut() {
                        o.remove("ServicePassword");
                    }
                    m
                })
                .collect::<Vec<_>>() })
        }
        "CreateVirtualMFADevice" => {
            let name = required(input, "VirtualMFADeviceName")?;
            let path = path_of(input)?;
            let serial = arn(ctx, "mfa", &path, name);
            if acct.virtual_mfa.contains_key(&serial) {
                return Err(exists(
                    "MFADevice entity at the same path and name already exists.",
                ));
            }
            let seed = random_chars(ctx, 64, UPPER);
            let d = json!({
                "SerialNumber": serial,
                "Base32StringSeed": STANDARD.encode(&seed),
                "QRCodePNG": STANDARD.encode(format!("otpauth://totp/{name}@{}?secret={seed}", ctx.account)),
            });
            let tags = tags_of(input)?;
            if !tags.is_empty() {
                acct.resource_tags
                    .insert(format!("MFADevice/{serial}"), tags.clone());
            }
            acct.virtual_mfa.insert(serial, d.clone());
            let mut out = d;
            if !tags.is_empty() {
                out["Tags"] = tag_values(&tags);
            }
            json!({ "VirtualMFADevice": out })
        }
        "EnableMFADevice" => {
            let user = required(input, "UserName")?.to_string();
            let serial = required(input, "SerialNumber")?.to_string();
            for k in ["AuthenticationCode1", "AuthenticationCode2"] {
                if !required(input, k)?.chars().all(|c| c.is_ascii_digit()) {
                    return Err(AwsError::new(
                        403,
                        "InvalidAuthenticationCode",
                        "Authentication code for device is not valid.",
                    ));
                }
            }
            if acct
                .users
                .values()
                .any(|u| u.mfa.iter().any(|(s, _)| *s == serial))
            {
                return Err(exists("MFA Device is already in use."));
            }
            if let Some(d) = acct.virtual_mfa.get_mut(&serial) {
                d["EnableDate"] = json!(t);
                d["User"] = json!({ "UserName": user });
            }
            acct.user_mut(&user)?.mfa.push((serial, t));
            json!({})
        }
        "DeactivateMFADevice" | "ResyncMFADevice" => {
            let serial = required(input, "SerialNumber")?.to_string();
            let user = acct
                .users
                .values_mut()
                .find(|u| {
                    u.mfa.iter().any(|(s, _)| *s == serial)
                        && input.str_field("UserName").is_none_or(|n| n == u.name)
                })
                .ok_or_else(|| {
                    no_such(format!(
                        "MFA Device with serial number {serial} cannot be found."
                    ))
                })?;
            if operation == "DeactivateMFADevice" {
                user.mfa.retain(|(s, _)| *s != serial);
                if let Some(d) = acct
                    .virtual_mfa
                    .get_mut(&serial)
                    .and_then(Value::as_object_mut)
                {
                    d.remove("EnableDate");
                    d.remove("User");
                }
            }
            json!({})
        }
        "DeleteVirtualMFADevice" => {
            let serial = required(input, "SerialNumber")?;
            if acct
                .users
                .values()
                .any(|u| u.mfa.iter().any(|(s, _)| s == serial))
            {
                return Err(conflict("MFA device is still enabled for a user."));
            }
            acct.virtual_mfa.remove(serial).ok_or_else(|| {
                no_such(format!(
                    "VirtualMFADevice with serial number {serial} doesn't exist."
                ))
            })?;
            json!({})
        }
        "ListMFADevices" => {
            let user = input.str_field("UserName");
            let devices = acct.users.values().filter(|u| user.is_none_or(|n| n == u.name)).flat_map(|u| u.mfa.iter().map(move |(s, at)| json!({ "UserName": u.name, "SerialNumber": s, "EnableDate": at }))).collect();
            page_value(devices, input, "MFADevices")
        }
        "ListVirtualMFADevices" => {
            let status = input.str_field("AssignmentStatus").unwrap_or("Any");
            let devices = acct
                .virtual_mfa
                .values()
                .filter(|d| match status {
                    "Assigned" => d.get("User").is_some(),
                    "Unassigned" => d.get("User").is_none(),
                    _ => true,
                })
                .map(|d| {
                    let mut v = json!({ "SerialNumber": d["SerialNumber"] });
                    for k in ["EnableDate", "User"] {
                        if let Some(x) = d.get(k) {
                            v[k] = x.clone();
                        }
                    }
                    v
                })
                .collect();
            page_value(devices, input, "VirtualMFADevices")
        }
        "GetMFADevice" => {
            let serial = required(input, "SerialNumber")?;
            let (user, at) = acct
                .users
                .values()
                .find_map(|u| {
                    u.mfa
                        .iter()
                        .find(|(s, _)| s == serial)
                        .map(|(_, at)| (u.name.clone(), *at))
                })
                .ok_or_else(|| {
                    no_such(format!(
                        "MFA Device with serial number {serial} cannot be found."
                    ))
                })?;
            json!({ "UserName": user, "SerialNumber": serial, "EnableDate": at })
        }
        "TagMFADevice"
        | "UntagMFADevice"
        | "ListMFADeviceTags"
        | "TagOpenIDConnectProvider"
        | "UntagOpenIDConnectProvider"
        | "ListOpenIDConnectProviderTags"
        | "TagSAMLProvider"
        | "UntagSAMLProvider"
        | "ListSAMLProviderTags"
        | "TagServerCertificate"
        | "UntagServerCertificate"
        | "ListServerCertificateTags" => {
            let (verb, rest) = if let Some(r) = operation.strip_prefix("Untag") {
                ("Untag", r)
            } else if let Some(r) = operation.strip_prefix("Tag") {
                ("Tag", r)
            } else {
                (
                    "",
                    operation
                        .trim_start_matches("List")
                        .trim_end_matches("Tags"),
                )
            };
            tag_op(tags_mut(acct, rest, input)?, verb, input)?
        }
        _ => return Ok(None),
    }))
}

fn credential_report(acct: &Account, ctx: &Ctx<'_>, at: f64) -> String {
    let iso = |t: f64| format_iso8601(t).replace(".000Z", "+00:00");
    let mut out = String::from(
        "user,arn,user_creation_time,password_enabled,password_last_used,password_last_changed,password_next_rotation,mfa_active,access_key_1_active,access_key_1_last_rotated,access_key_1_last_used_date,access_key_1_last_used_region,access_key_1_last_used_service,access_key_2_active,access_key_2_last_rotated,access_key_2_last_used_date,access_key_2_last_used_region,access_key_2_last_used_service,cert_1_active,cert_1_last_rotated,cert_2_active,cert_2_last_rotated\n",
    );
    out.push_str(&format!("<root_account>,arn:aws:iam::{}:root,{},not_supported,N/A,not_supported,not_supported,false,false,N/A,N/A,N/A,N/A,false,N/A,N/A,N/A,N/A,false,N/A,false,N/A\n", ctx.account, iso(at)));
    for u in acct.users.values() {
        let keys: Vec<&AccessKey> = acct
            .access_keys
            .iter()
            .filter(|k| k.user == u.name)
            .collect();
        let key = |i: usize| {
            keys.get(i)
                .map_or("false,N/A,N/A,N/A,N/A".to_string(), |k| {
                    format!("{},{},N/A,N/A,N/A", k.status == "Active", iso(k.created))
                })
        };
        let certs: Vec<&Value> = acct
            .signing_certs
            .iter()
            .filter(|c| c["UserName"] == u.name.as_str())
            .collect();
        let cert = |i: usize| {
            certs.get(i).map_or("false,N/A".to_string(), |c| {
                format!(
                    "{},{}",
                    c["Status"] == "Active",
                    iso(c["UploadDate"].as_f64().unwrap_or_default())
                )
            })
        };
        out.push_str(&format!(
            "{},{},{},{},N/A,{},N/A,{},{},{},{},{}\n",
            u.name,
            arn(ctx, "user", &u.path, &u.name),
            iso(u.created),
            u.login.is_some(),
            u.login.map_or("N/A".to_string(), |(t, _)| iso(t)),
            !u.mfa.is_empty(),
            key(0),
            key(1),
            cert(0),
            cert(1),
        ));
    }
    out
}

fn summary(acct: &Account) -> Value {
    let attached: usize = acct.users.values().map(|u| u.attached.len()).sum::<usize>()
        + acct
            .groups
            .values()
            .map(|g| g.attached.len())
            .sum::<usize>()
        + acct.roles.values().map(|r| r.attached.len()).sum::<usize>();
    let mut m = Map::new();
    for (k, v) in [
        ("Users", acct.users.len()),
        ("UsersQuota", 5000),
        ("Groups", acct.groups.len()),
        ("GroupsQuota", 300),
        ("Roles", acct.roles.len()),
        ("RolesQuota", 1000),
        ("Policies", acct.policies.len()),
        ("PoliciesQuota", 1500),
        ("InstanceProfiles", acct.profiles.len()),
        ("InstanceProfilesQuota", 1000),
        ("ServerCertificates", acct.server_certs.len()),
        ("ServerCertificatesQuota", 20),
        ("MFADevices", acct.virtual_mfa.len()),
        (
            "MFADevicesInUse",
            acct.users.values().map(|u| u.mfa.len()).sum(),
        ),
        ("AccountMFAEnabled", 0),
        ("AccountAccessKeysPresent", 0),
        ("AccountSigningCertificatesPresent", 0),
        ("Providers", acct.oidc.len() + acct.saml.len()),
        ("AttachedPoliciesPerUserQuota", 10),
        ("AttachedPoliciesPerGroupQuota", 10),
        ("AttachedPoliciesPerRoleQuota", 10),
        ("PolicySizeQuota", 6144),
        ("PolicyVersionsInUse", attached),
        ("PolicyVersionsInUseQuota", 10000),
        ("VersionsPerPolicyQuota", 5),
        ("GroupsPerUserQuota", 10),
        ("AccessKeysPerUserQuota", 2),
        ("SigningCertificatesPerUserQuota", 2),
        ("UserPolicySizeQuota", 2048),
        ("GroupPolicySizeQuota", 5120),
        ("RolePolicySizeQuota", 10240),
        ("AssumeRolePolicySizeQuota", 2048),
        ("GlobalEndpointTokenVersion", 1),
    ] {
        m.insert(k.to_string(), json!(v));
    }
    Value::Object(m)
}

pub fn call(acct: &mut Account, ctx: &mut Ctx<'_>, operation: &str, input: &Value) -> Handled {
    let t = now(ctx);
    Ok(Some(match operation {
        "CreateAccountAlias" => {
            let alias = required(input, "AccountAlias")?;
            if !alias
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                || alias.starts_with('-')
                || alias.ends_with('-')
            {
                return Err(AwsError::new(
                    400,
                    "ValidationError",
                    format!(
                        "The specified value for accountAlias is invalid. It must contain only digits, lowercase letters, and hyphens: {alias}"
                    ),
                ));
            }
            acct.aliases = vec![alias.to_string()];
            json!({})
        }
        "DeleteAccountAlias" => {
            let alias = required(input, "AccountAlias")?;
            if !acct.aliases.iter().any(|a| a == alias) {
                return Err(no_such(format!(
                    "The account alias {alias} cannot be found."
                )));
            }
            acct.aliases.clear();
            json!({})
        }
        "ListAccountAliases" => page_value(
            acct.aliases.iter().map(|a| json!(a)).collect(),
            input,
            "AccountAliases",
        ),
        "UpdateAccountPasswordPolicy" => {
            let mut p = json!({
                "MinimumPasswordLength": 6,
                "RequireSymbols": false,
                "RequireNumbers": false,
                "RequireUppercaseCharacters": false,
                "RequireLowercaseCharacters": false,
                "AllowUsersToChangePassword": false,
                "ExpirePasswords": false,
                "HardExpiry": false,
            });
            for (k, v) in input.as_object().into_iter().flatten() {
                p[k] = v.clone();
            }
            p["ExpirePasswords"] = json!(
                p.get("MaxPasswordAge")
                    .and_then(Value::as_i64)
                    .is_some_and(|d| d > 0)
            );
            acct.password_policy = Some(p);
            json!({})
        }
        "GetAccountPasswordPolicy" => {
            let p = acct.password_policy.clone().ok_or_else(|| {
                no_such(format!(
                    "The Password Policy with domain name {} cannot be found.",
                    ctx.account
                ))
            })?;
            json!({ "PasswordPolicy": p })
        }
        "DeleteAccountPasswordPolicy" => {
            acct.password_policy.take().ok_or_else(|| {
                no_such("The account policy with name PasswordPolicy cannot be found.")
            })?;
            json!({})
        }
        "GetAccountSummary" => json!({ "SummaryMap": summary(acct) }),
        "GenerateCredentialReport" => {
            let state = if acct.credential_report.is_some() {
                "COMPLETE"
            } else {
                "STARTED"
            };
            acct.credential_report = Some(t);
            json!({ "State": state, "Description": if state == "STARTED" { "No report exists. Starting a new report generation task" } else { "" } })
        }
        "GetCredentialReport" => {
            let at = acct.credential_report.ok_or_else(|| {
                AwsError::new(
                    410,
                    "ReportNotPresent",
                    "Credential report not present. Use GenerateCredentialReport to generate one.",
                )
            })?;
            json!({ "Content": STANDARD.encode(credential_report(acct, ctx, at)), "ReportFormat": "text/csv", "GeneratedTime": at })
        }
        "SetSecurityTokenServicePreferences" => {
            acct.sts_token_version =
                Some(required(input, "GlobalEndpointTokenVersion")?.to_string());
            json!({})
        }
        "CreateOpenIDConnectProvider" => {
            let url = required(input, "Url")?;
            if !url.starts_with("https://") {
                return Err(AwsError::new(
                    400,
                    "ValidationError",
                    "Invalid Open ID Connect Provider URL",
                ));
            }
            let host = url.trim_start_matches("https://").trim_end_matches('/');
            let arn = format!("arn:aws:iam::{}:oidc-provider/{host}", ctx.account);
            if acct.oidc.contains_key(&arn) {
                return Err(exists(format!("Provider with url {url} already exists.")));
            }
            let tags = tags_of(input)?;
            acct.oidc.insert(
                arn.clone(),
                json!({
                    "Url": host,
                    "ClientIDList": strings(input.array_field("ClientIDList")),
                    "ThumbprintList": strings(input.array_field("ThumbprintList")),
                    "CreateDate": t,
                }),
            );
            if !tags.is_empty() {
                acct.resource_tags
                    .insert(format!("OpenIDConnectProvider/{arn}"), tags.clone());
            }
            json!({ "OpenIDConnectProviderArn": arn, "Tags": tag_values(&tags) })
        }
        "GetOpenIDConnectProvider" => {
            let arn = required(input, "OpenIDConnectProviderArn")?;
            let mut p = acct.oidc.get(arn).cloned().ok_or_else(|| {
                no_such(format!("OpenIDConnect Provider not found for arn {arn}"))
            })?;
            p["Tags"] = tag_values(
                acct.resource_tags
                    .get(&format!("OpenIDConnectProvider/{arn}"))
                    .unwrap_or(&Vec::new()),
            );
            p
        }
        "DeleteOpenIDConnectProvider" => {
            let arn = required(input, "OpenIDConnectProviderArn")?;
            acct.oidc.remove(arn);
            acct.resource_tags
                .remove(&format!("OpenIDConnectProvider/{arn}"));
            json!({})
        }
        "ListOpenIDConnectProviders" => {
            json!({ "OpenIDConnectProviderList": acct.oidc.keys().map(|a| json!({ "Arn": a })).collect::<Vec<_>>() })
        }
        "AddClientIDToOpenIDConnectProvider"
        | "RemoveClientIDFromOpenIDConnectProvider"
        | "UpdateOpenIDConnectProviderThumbprint" => {
            let arn = required(input, "OpenIDConnectProviderArn")?;
            let p = acct.oidc.get_mut(arn).ok_or_else(|| {
                no_such(format!("OpenIDConnect Provider not found for arn {arn}"))
            })?;
            match operation {
                "UpdateOpenIDConnectProviderThumbprint" => {
                    p["ThumbprintList"] = json!(strings(input.array_field("ThumbprintList")))
                }
                op => {
                    let id = required(input, "ClientID")?;
                    let mut ids =
                        strings(p["ClientIDList"].as_array().map_or(&[][..], Vec::as_slice));
                    ids.retain(|x| x != id);
                    if op.starts_with("Add") {
                        ids.push(id.to_string());
                    }
                    p["ClientIDList"] = json!(ids);
                }
            }
            json!({})
        }
        "CreateSAMLProvider" => {
            let name = required(input, "Name")?;
            let doc = required(input, "SAMLMetadataDocument")?;
            if doc.len() < 1000 && !doc.contains("EntityDescriptor") {
                return Err(invalid("Could not parse metadata"));
            }
            let arn = format!("arn:aws:iam::{}:saml-provider/{name}", ctx.account);
            if acct.saml.contains_key(&arn) {
                return Err(exists(format!("SAMLProvider {name} already exists")));
            }
            let tags = tags_of(input)?;
            acct.saml.insert(arn.clone(), json!({
                "SAMLMetadataDocument": doc,
                "CreateDate": t,
                "ValidUntil": t + 100.0 * 365.0 * 86400.0,
                "SAMLProviderUUID": ctx.uuid(),
                "AssertionEncryptionMode": input.str_field("AssertionEncryptionMode").unwrap_or("Allowed"),
            }));
            if !tags.is_empty() {
                acct.resource_tags
                    .insert(format!("SAMLProvider/{arn}"), tags.clone());
            }
            json!({ "SAMLProviderArn": arn, "Tags": tag_values(&tags) })
        }
        "GetSAMLProvider" => {
            let arn = required(input, "SAMLProviderArn")?;
            let mut p = acct
                .saml
                .get(arn)
                .cloned()
                .ok_or_else(|| no_such(format!("SAMLProvider {arn} not found")))?;
            p["Tags"] = tag_values(
                acct.resource_tags
                    .get(&format!("SAMLProvider/{arn}"))
                    .unwrap_or(&Vec::new()),
            );
            p
        }
        "UpdateSAMLProvider" => {
            let arn = required(input, "SAMLProviderArn")?.to_string();
            let p = acct
                .saml
                .get_mut(&arn)
                .ok_or_else(|| no_such(format!("SAMLProvider {arn} not found")))?;
            if let Some(d) = input.str_field("SAMLMetadataDocument") {
                p["SAMLMetadataDocument"] = json!(d);
            }
            if let Some(m) = input.str_field("AssertionEncryptionMode") {
                p["AssertionEncryptionMode"] = json!(m);
            }
            json!({ "SAMLProviderArn": arn })
        }
        "DeleteSAMLProvider" => {
            let arn = required(input, "SAMLProviderArn")?;
            acct.saml
                .remove(arn)
                .ok_or_else(|| no_such(format!("SAMLProvider {arn} not found")))?;
            json!({})
        }
        "ListSAMLProviders" => {
            json!({ "SAMLProviderList": acct.saml.iter().map(|(a, p)| json!({ "Arn": a, "ValidUntil": p["ValidUntil"], "CreateDate": p["CreateDate"] })).collect::<Vec<_>>() })
        }
        "UploadServerCertificate" => {
            let name = required(input, "ServerCertificateName")?.to_string();
            if acct.server_certs.contains_key(&name) {
                return Err(exists(format!(
                    "The Server Certificate with name {name} already exists."
                )));
            }
            let body = required(input, "CertificateBody")?;
            required(input, "PrivateKey")?;
            if !body.contains("BEGIN CERTIFICATE") {
                return Err(AwsError::new(
                    400,
                    "MalformedCertificate",
                    "Certificate body is not a PEM-encoded certificate.",
                ));
            }
            let path = path_of(input)?;
            let meta = json!({
                "Path": path,
                "ServerCertificateName": name,
                "ServerCertificateId": unique_id(ctx, "ASCA"),
                "Arn": arn(ctx, "server-certificate", &path, &name),
                "UploadDate": t,
                "Expiration": t + 365.0 * 86400.0,
            });
            let tags = tags_of(input)?;
            if !tags.is_empty() {
                acct.resource_tags
                    .insert(format!("ServerCertificate/{name}"), tags.clone());
            }
            acct.server_certs.insert(name, json!({ "Meta": meta, "CertificateBody": body, "CertificateChain": input.str_field("CertificateChain") }));
            json!({ "ServerCertificateMetadata": meta, "Tags": tag_values(&tags) })
        }
        "GetServerCertificate" => {
            let name = required(input, "ServerCertificateName")?;
            let c = acct.server_certs.get(name).ok_or_else(|| {
                no_such(format!(
                    "The Server Certificate with name {name} cannot be found."
                ))
            })?;
            let mut out = json!({ "ServerCertificateMetadata": c["Meta"], "CertificateBody": c["CertificateBody"] });
            if let Some(chain) = c["CertificateChain"].as_str() {
                out["CertificateChain"] = json!(chain);
            }
            out["Tags"] = tag_values(
                acct.resource_tags
                    .get(&format!("ServerCertificate/{name}"))
                    .unwrap_or(&Vec::new()),
            );
            json!({ "ServerCertificate": out })
        }
        "ListServerCertificates" => {
            let prefix = input.str_field("PathPrefix").unwrap_or("/");
            let certs = acct
                .server_certs
                .values()
                .filter(|c| {
                    c["Meta"]["Path"]
                        .as_str()
                        .is_some_and(|p| p.starts_with(prefix))
                })
                .map(|c| c["Meta"].clone())
                .collect();
            page_value(certs, input, "ServerCertificateMetadataList")
        }
        "UpdateServerCertificate" => {
            let name = required(input, "ServerCertificateName")?.to_string();
            let mut c = acct.server_certs.remove(&name).ok_or_else(|| {
                no_such(format!(
                    "The Server Certificate with name {name} cannot be found."
                ))
            })?;
            let new_name = input
                .str_field("NewServerCertificateName")
                .unwrap_or(&name)
                .to_string();
            if new_name != name && acct.server_certs.contains_key(&new_name) {
                acct.server_certs.insert(name, c);
                return Err(exists(format!(
                    "The Server Certificate with name {new_name} already exists."
                )));
            }
            let path = input
                .str_field("NewPath")
                .map(str::to_string)
                .unwrap_or_else(|| c["Meta"]["Path"].as_str().unwrap_or("/").to_string());
            c["Meta"]["ServerCertificateName"] = json!(new_name);
            c["Meta"]["Path"] = json!(path);
            c["Meta"]["Arn"] = json!(arn(ctx, "server-certificate", &path, &new_name));
            if let Some(tags) = acct
                .resource_tags
                .remove(&format!("ServerCertificate/{name}"))
            {
                acct.resource_tags
                    .insert(format!("ServerCertificate/{new_name}"), tags);
            }
            acct.server_certs.insert(new_name, c);
            json!({})
        }
        "DeleteServerCertificate" => {
            let name = required(input, "ServerCertificateName")?;
            acct.server_certs.remove(name).ok_or_else(|| {
                no_such(format!(
                    "The Server Certificate with name {name} cannot be found."
                ))
            })?;
            acct.resource_tags
                .remove(&format!("ServerCertificate/{name}"));
            json!({})
        }
        _ => return Ok(None),
    }))
}

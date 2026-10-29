use serde_json::{Value, json};

use super::policy::{self, Context, Source};
use super::principals::{group_view, profile_view, role_view, user_view};
use super::{
    Account, Handled, ManagedPolicy, conflict, encode_doc, exists, invalid, name_from_arn, no_such,
    now, page_value, path_of, required, strings, tag_op, tag_values, tags_of, unique_id, validated,
};
use crate::codec::AwsError;
use crate::service::{Ctx, InputExt};

const ADMIN: &str =
    r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"*","Resource":"*"}]}"#;

/// AWS managed policies addressable as `arn:aws:iam::aws:policy/<path><name>`.
const AWS_MANAGED: &[(&str, &str, &str)] = &[
    ("/", "AdministratorAccess", ADMIN),
    (
        "/job-function/",
        "ViewOnlyAccess",
        r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["*:List*","*:Describe*","*:Get*"],"Resource":"*"}]}"#,
    ),
    (
        "/",
        "PowerUserAccess",
        r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","NotAction":["iam:*","organizations:*","account:*"],"Resource":"*"}]}"#,
    ),
    (
        "/",
        "ReadOnlyAccess",
        r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["*:Describe*","*:Get*","*:List*","s3:Head*"],"Resource":"*"}]}"#,
    ),
    (
        "/",
        "IAMFullAccess",
        r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"iam:*","Resource":"*"}]}"#,
    ),
    (
        "/",
        "AmazonS3FullAccess",
        r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["s3:*","s3-object-lambda:*"],"Resource":"*"}]}"#,
    ),
    (
        "/",
        "AmazonS3ReadOnlyAccess",
        r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["s3:Get*","s3:List*","s3:Describe*","s3-object-lambda:Get*","s3-object-lambda:List*"],"Resource":"*"}]}"#,
    ),
    (
        "/",
        "AmazonDynamoDBFullAccess",
        r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"dynamodb:*","Resource":"*"}]}"#,
    ),
    (
        "/",
        "AmazonDynamoDBReadOnlyAccess",
        r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["dynamodb:BatchGetItem","dynamodb:Describe*","dynamodb:List*","dynamodb:GetItem","dynamodb:Query","dynamodb:Scan","dynamodb:PartiQLSelect"],"Resource":"*"}]}"#,
    ),
    (
        "/",
        "AmazonSQSFullAccess",
        r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"sqs:*","Resource":"*"}]}"#,
    ),
    (
        "/",
        "AmazonSNSFullAccess",
        r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"sns:*","Resource":"*"}]}"#,
    ),
    (
        "/",
        "AmazonKinesisFullAccess",
        r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"kinesis:*","Resource":"*"}]}"#,
    ),
    (
        "/",
        "CloudWatchFullAccess",
        r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["cloudwatch:*","logs:*","sns:*"],"Resource":"*"}]}"#,
    ),
    (
        "/",
        "CloudWatchLogsFullAccess",
        r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"logs:*","Resource":"*"}]}"#,
    ),
    (
        "/",
        "SecretsManagerReadWrite",
        r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"secretsmanager:*","Resource":"*"}]}"#,
    ),
    (
        "/service-role/",
        "AWSLambdaBasicExecutionRole",
        r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["logs:CreateLogGroup","logs:CreateLogStream","logs:PutLogEvents"],"Resource":"*"}]}"#,
    ),
];

pub fn aws_managed(arn: &str) -> Option<ManagedPolicy> {
    let rest = arn.strip_prefix("arn:aws:iam::aws:policy")?;
    AWS_MANAGED
        .iter()
        .find(|(path, name, _)| rest == format!("{path}{name}"))
        .map(|(path, name, doc)| ManagedPolicy {
            name: name.to_string(),
            id: format!(
                "ANPA{}",
                name.to_uppercase()
                    .chars()
                    .filter(char::is_ascii_alphanumeric)
                    .take(17)
                    .collect::<String>()
            ),
            path: path.to_string(),
            arn: arn.to_string(),
            description: None,
            created: 1_423_000_000.0,
            updated: 1_423_000_000.0,
            default_version: "v1".to_string(),
            versions: vec![("v1".to_string(), doc.to_string(), 1_423_000_000.0)],
            next_version: 2,
            tags: Vec::new(),
        })
}

fn policy_view(acct: &Account, p: &ManagedPolicy, tags: bool) -> Value {
    let mut v = json!({
        "PolicyName": p.name,
        "PolicyId": p.id,
        "Arn": p.arn,
        "Path": p.path,
        "DefaultVersionId": p.default_version,
        "AttachmentCount": acct.attachment_count(&p.arn),
        "PermissionsBoundaryUsageCount": acct.boundary_count(&p.arn),
        "IsAttachable": true,
        "CreateDate": p.created,
        "UpdateDate": p.updated,
    });
    if let Some(d) = &p.description {
        v["Description"] = json!(d);
    }
    if tags && !p.tags.is_empty() {
        v["Tags"] = tag_values(&p.tags);
    }
    v
}

fn version_view(p: &ManagedPolicy, (id, doc, at): &(String, String, f64)) -> Value {
    json!({
        "Document": encode_doc(doc),
        "VersionId": id,
        "IsDefaultVersion": *id == p.default_version,
        "CreateDate": at,
    })
}

fn own_policy<'a>(acct: &'a mut Account, arn: &str) -> Result<&'a mut ManagedPolicy, AwsError> {
    acct.policies
        .get_mut(arn)
        .ok_or_else(|| no_such(format!("Policy {arn} was not found.")))
}

fn sources(docs: impl IntoIterator<Item = (String, String)>) -> Vec<Source> {
    docs.into_iter()
        .filter_map(|(id, doc)| {
            Some(Source {
                doc: policy::parse(&doc).ok()?,
                id,
                kind: "IAMPolicy".to_string(),
            })
        })
        .collect()
}

fn managed_docs(acct: &Account, arns: &[String]) -> Vec<(String, String)> {
    arns.iter()
        .filter_map(|a| {
            let p = acct.policy(a).ok()?;
            let doc = p
                .versions
                .iter()
                .find(|v| v.0 == p.default_version)?
                .1
                .clone();
            Some((a.clone(), doc))
        })
        .collect()
}

/// Identity policies of the user, group, or role behind `arn`, and its boundary.
fn principal_policies(
    acct: &Account,
    arn: &str,
) -> Result<(Vec<Source>, Option<Source>), AwsError> {
    let name = name_from_arn(arn);
    let mut docs: Vec<(String, String)> = Vec::new();
    let boundary = if arn.contains(":user/") {
        let u = acct.user(name)?;
        docs.extend(u.inline.iter().map(|(n, d)| (n.clone(), d.clone())));
        docs.extend(managed_docs(acct, &u.attached));
        for g in acct.groups.values().filter(|g| g.members.contains(&u.name)) {
            docs.extend(g.inline.iter().map(|(n, d)| (n.clone(), d.clone())));
            docs.extend(managed_docs(acct, &g.attached));
        }
        u.boundary.clone()
    } else if arn.contains(":role/") {
        let r = acct.role(name)?;
        docs.extend(r.inline.iter().map(|(n, d)| (n.clone(), d.clone())));
        docs.extend(managed_docs(acct, &r.attached));
        r.boundary.clone()
    } else if arn.contains(":group/") {
        let g = acct
            .groups
            .get(name)
            .ok_or_else(|| no_such(format!("The group with name {name} cannot be found.")))?;
        docs.extend(g.inline.iter().map(|(n, d)| (n.clone(), d.clone())));
        docs.extend(managed_docs(acct, &g.attached));
        None
    } else {
        return Err(invalid(format!("Invalid ARN {arn}.")));
    };
    let boundary = boundary
        .map(|b| managed_docs(acct, &[b]))
        .and_then(|d| sources(d).into_iter().next())
        .map(|mut s| {
            s.kind = "PermissionsBoundaryPolicy".to_string();
            s
        });
    Ok((sources(docs), boundary))
}

fn context(input: &Value) -> Context {
    let mut ctx = Context::new();
    for e in input.array_field("ContextEntries") {
        if let Some(k) = e.str_field("ContextKeyName") {
            ctx.insert(k.to_string(), strings(e.array_field("ContextKeyValues")));
        }
    }
    ctx
}

fn custom(input: &Value, key: &str) -> Result<Vec<Source>, AwsError> {
    strings(input.array_field(key))
        .iter()
        .enumerate()
        .map(|(i, doc)| {
            Ok(Source {
                doc: policy::parse(doc)
                    .map_err(|m| AwsError::new(400, "MalformedPolicyDocument", m))?,
                id: format!("PolicyInputList.{}", i + 1),
                kind: "IAMPolicy".to_string(),
            })
        })
        .collect()
}

fn simulate(
    policies: &[Source],
    boundary: Option<&Source>,
    input: &Value,
) -> Result<Value, AwsError> {
    let actions = strings(input.array_field("ActionNames"));
    if actions.is_empty() {
        return Err(invalid("ActionNames must contain at least one action."));
    }
    let mut resources = strings(input.array_field("ResourceArns"));
    if resources.is_empty() {
        resources.push("*".to_string());
    }
    let ctx = context(input);
    let mut results = Vec::new();
    for a in &actions {
        for r in &resources {
            let out = policy::evaluate(policies, boundary, a, r, &ctx);
            let mut v = json!({
                "EvalActionName": a,
                "EvalResourceName": r,
                "EvalDecision": out.decision,
                "MatchedStatements": out.matched,
                "MissingContextValues": [],
            });
            if let Some(b) = boundary {
                let allowed = policy::evaluate(std::slice::from_ref(b), None, a, r, &ctx).decision
                    == "allowed";
                v["PermissionsBoundaryDecisionDetail"] =
                    json!({ "AllowedByPermissionsBoundary": allowed });
            }
            results.push(v);
        }
    }
    Ok(page_value(results, input, "EvaluationResults"))
}

pub fn call(acct: &mut Account, ctx: &mut Ctx<'_>, operation: &str, input: &Value) -> Handled {
    let t = now(ctx);
    Ok(Some(match operation {
        "CreatePolicy" => {
            let name = required(input, "PolicyName")?.to_string();
            let path = path_of(input)?;
            let arn = super::arn(ctx, "policy", &path, &name);
            if acct.policies.contains_key(&arn) {
                return Err(exists(format!(
                    "A policy called {name} already exists. Duplicate names are not allowed."
                )));
            }
            let p = ManagedPolicy {
                id: unique_id(ctx, "ANPA"),
                description: input.str_field("Description").map(str::to_string),
                created: t,
                updated: t,
                default_version: "v1".to_string(),
                versions: vec![("v1".to_string(), validated(input, "PolicyDocument")?, t)],
                next_version: 2,
                tags: tags_of(input)?,
                name,
                path,
                arn: arn.clone(),
            };
            let out = json!({ "Policy": policy_view(acct, &p, true) });
            acct.policies.insert(arn, p);
            out
        }
        "GetPolicy" => {
            let p = acct.policy(required(input, "PolicyArn")?)?;
            json!({ "Policy": policy_view(acct, &p, true) })
        }
        "DeletePolicy" => {
            let arn = required(input, "PolicyArn")?;
            own_policy(acct, arn)?;
            if acct.attachment_count(arn) > 0 {
                return Err(conflict("Cannot delete a policy attached to entities."));
            }
            if acct.policies[arn].versions.len() > 1 {
                return Err(conflict(
                    "Cannot delete a policy with non-default versions; delete them first.",
                ));
            }
            acct.policies.remove(arn);
            json!({})
        }
        "ListPolicies" => {
            let scope = input.str_field("Scope").unwrap_or("All");
            let prefix = input.str_field("PathPrefix").unwrap_or("/");
            let only_attached = input.get("OnlyAttached").and_then(Value::as_bool) == Some(true);
            let mut list: Vec<ManagedPolicy> = Vec::new();
            if scope != "Local" {
                list.extend(AWS_MANAGED.iter().filter_map(|(p, n, _)| {
                    aws_managed(&format!("arn:aws:iam::aws:policy{p}{n}"))
                }));
            }
            if scope != "AWS" {
                list.extend(acct.policies.values().cloned());
            }
            let views = list
                .iter()
                .filter(|p| p.path.starts_with(prefix))
                .filter(|p| !only_attached || acct.attachment_count(&p.arn) > 0)
                .map(|p| policy_view(acct, p, false))
                .collect();
            page_value(views, input, "Policies")
        }
        "CreatePolicyVersion" => {
            let arn = required(input, "PolicyArn")?.to_string();
            let doc = validated(input, "PolicyDocument")?;
            let p = own_policy(acct, &arn)?;
            if p.versions.len() >= 5 {
                return Err(AwsError::new(
                    409,
                    "LimitExceeded",
                    "A managed policy can have up to 5 versions. Before you create a new version, you must delete an existing version.",
                ));
            }
            let id = format!("v{}", p.next_version);
            p.next_version += 1;
            p.versions.push((id.clone(), doc, t));
            if input.get("SetAsDefault").and_then(Value::as_bool) == Some(true) {
                p.default_version = id.clone();
                p.updated = t;
            }
            let v = p.versions.last().cloned().expect("version was pushed");
            json!({ "PolicyVersion": version_view(p, &v) })
        }
        "GetPolicyVersion" => {
            let p = acct.policy(required(input, "PolicyArn")?)?;
            let id = required(input, "VersionId")?;
            let v = p.versions.iter().find(|v| v.0 == id).ok_or_else(|| {
                no_such(format!(
                    "Policy {} version {id} does not exist or is not attachable.",
                    p.arn
                ))
            })?;
            json!({ "PolicyVersion": version_view(&p, v) })
        }
        "ListPolicyVersions" => {
            let p = acct.policy(required(input, "PolicyArn")?)?;
            let mut vs: Vec<Value> = p.versions.iter().map(|v| version_view(&p, v)).collect();
            vs.reverse();
            page_value(vs, input, "Versions")
        }
        "DeletePolicyVersion" => {
            let arn = required(input, "PolicyArn")?;
            let id = required(input, "VersionId")?;
            let p = own_policy(acct, arn)?;
            if p.default_version == id {
                return Err(conflict("Cannot delete the default version of a policy."));
            }
            let before = p.versions.len();
            p.versions.retain(|v| v.0 != id);
            if p.versions.len() == before {
                return Err(no_such(format!(
                    "Policy {arn} version {id} does not exist."
                )));
            }
            json!({})
        }
        "SetDefaultPolicyVersion" => {
            let arn = required(input, "PolicyArn")?;
            let id = required(input, "VersionId")?;
            let p = own_policy(acct, arn)?;
            if !p.versions.iter().any(|v| v.0 == id) {
                return Err(no_such(format!(
                    "Policy {arn} version {id} does not exist."
                )));
            }
            p.default_version = id.to_string();
            p.updated = t;
            json!({})
        }
        "TagPolicy" | "UntagPolicy" | "ListPolicyTags" => {
            let p = own_policy(acct, required(input, "PolicyArn")?)?;
            let verb = operation[..operation.find("Policy").unwrap_or(0)].replace("List", "");
            tag_op(&mut p.tags, &verb, input)?
        }
        "ListEntitiesForPolicy" => {
            let arn = required(input, "PolicyArn")?;
            acct.policy(arn)?;
            let filter = input.str_field("EntityFilter");
            let want = |k: &str| filter.is_none_or(|f| f == k);
            let usage = input
                .str_field("PolicyUsageFilter")
                .unwrap_or("PermissionsPolicy");
            let has = |attached: &Vec<String>, boundary: Option<&String>| {
                if usage == "PermissionsBoundary" {
                    boundary.is_some_and(|b| b == arn)
                } else {
                    attached.iter().any(|a| a == arn)
                }
            };
            let mut out = json!({
                "PolicyGroups": if want("Group") { acct.groups.values().filter(|g| has(&g.attached, None)).map(|g| json!({ "GroupName": g.name, "GroupId": g.id })).collect::<Vec<_>>() } else { Vec::new() },
                "PolicyUsers": if want("User") { acct.users.values().filter(|u| has(&u.attached, u.boundary.as_ref())).map(|u| json!({ "UserName": u.name, "UserId": u.id })).collect::<Vec<_>>() } else { Vec::new() },
                "PolicyRoles": if want("Role") { acct.roles.values().filter(|r| has(&r.attached, r.boundary.as_ref())).map(|r| json!({ "RoleName": r.name, "RoleId": r.id })).collect::<Vec<_>>() } else { Vec::new() },
            });
            out["IsTruncated"] = json!(false);
            out
        }
        "GetAccountAuthorizationDetails" => {
            let filter = strings(input.array_field("Filter"));
            let want = |k: &str| filter.is_empty() || filter.iter().any(|f| f == k);
            let attached = |l: &Vec<String>| -> Vec<Value> {
                l.iter()
                    .filter_map(|a| acct.policy(a).ok())
                    .map(|p| json!({ "PolicyName": p.name, "PolicyArn": p.arn }))
                    .collect()
            };
            let inline = |m: &std::collections::BTreeMap<String, String>| -> Vec<Value> {
                m.iter()
                    .map(|(n, d)| json!({ "PolicyName": n, "PolicyDocument": encode_doc(d) }))
                    .collect()
            };
            let mut out = json!({ "IsTruncated": false });
            if want("User") {
                out["UserDetailList"] = acct
                    .users
                    .values()
                    .map(|u| {
                        let mut v = user_view(ctx, u, true);
                        v["UserPolicyList"] = json!(inline(&u.inline));
                        v["AttachedManagedPolicies"] = json!(attached(&u.attached));
                        v["GroupList"] = json!(
                            acct.groups
                                .values()
                                .filter(|g| g.members.contains(&u.name))
                                .map(|g| g.name.clone())
                                .collect::<Vec<_>>()
                        );
                        v
                    })
                    .collect();
            }
            if want("Group") {
                out["GroupDetailList"] = acct
                    .groups
                    .values()
                    .map(|g| {
                        let mut v = group_view(ctx, g);
                        v["GroupPolicyList"] = json!(inline(&g.inline));
                        v["AttachedManagedPolicies"] = json!(attached(&g.attached));
                        v
                    })
                    .collect();
            }
            if want("Role") {
                out["RoleDetailList"] = acct
                    .roles
                    .values()
                    .map(|r| {
                        let mut v = role_view(ctx, r, true);
                        v["RolePolicyList"] = json!(inline(&r.inline));
                        v["AttachedManagedPolicies"] = json!(attached(&r.attached));
                        v["InstanceProfileList"] = json!(
                            acct.profiles
                                .values()
                                .filter(|p| p.roles.contains(&r.name))
                                .map(|p| profile_view(ctx, acct, p))
                                .collect::<Vec<_>>()
                        );
                        v
                    })
                    .collect();
            }
            if want("LocalManagedPolicy") || want("AWSManagedPolicy") {
                let mut list: Vec<ManagedPolicy> = Vec::new();
                if want("LocalManagedPolicy") {
                    list.extend(acct.policies.values().cloned());
                }
                if filter.iter().any(|f| f == "AWSManagedPolicy") {
                    list.extend(AWS_MANAGED.iter().filter_map(|(p, n, _)| {
                        aws_managed(&format!("arn:aws:iam::aws:policy{p}{n}"))
                    }));
                }
                out["Policies"] = list
                    .iter()
                    .map(|p| {
                        let mut v = policy_view(acct, p, false);
                        v["PolicyVersionList"] = json!(
                            p.versions
                                .iter()
                                .map(|x| version_view(p, x))
                                .collect::<Vec<_>>()
                        );
                        v
                    })
                    .collect();
            }
            out
        }
        "SimulatePrincipalPolicy" => {
            let arn = required(input, "PolicySourceArn")?;
            let (mut policies, boundary) = principal_policies(acct, arn)?;
            policies.extend(custom(input, "PolicyInputList")?);
            let boundary = custom(input, "PermissionsBoundaryPolicyInputList")?
                .into_iter()
                .next()
                .or(boundary);
            simulate(&policies, boundary.as_ref(), input)?
        }
        "SimulateCustomPolicy" => {
            let policies = custom(input, "PolicyInputList")?;
            if policies.is_empty() {
                return Err(invalid("PolicyInputList must contain at least one policy."));
            }
            let boundary = custom(input, "PermissionsBoundaryPolicyInputList")?
                .into_iter()
                .next();
            simulate(&policies, boundary.as_ref(), input)?
        }
        "GetContextKeysForCustomPolicy" | "GetContextKeysForPrincipalPolicy" => {
            let mut docs = custom(input, "PolicyInputList")?;
            if let Some(arn) = input.str_field("PolicySourceArn") {
                let (p, b) = principal_policies(acct, arn)?;
                docs.extend(p);
                docs.extend(b);
            }
            let mut keys: Vec<String> = docs
                .iter()
                .flat_map(|s| policy::context_keys(&s.doc))
                .collect();
            keys.sort_by_key(|k| k.to_lowercase());
            keys.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
            json!({ "ContextKeyNames": keys })
        }
        "GenerateServiceLastAccessedDetails" => {
            let arn = required(input, "Arn")?;
            if acct
                .users
                .values()
                .all(|u| !arn.ends_with(&format!("user{}{}", u.path, u.name)))
                && acct
                    .roles
                    .values()
                    .all(|r| !arn.ends_with(&format!("role{}{}", r.path, r.name)))
                && acct
                    .groups
                    .values()
                    .all(|g| !arn.ends_with(&format!("group{}{}", g.path, g.name)))
                && !acct.policies.contains_key(arn)
            {
                return Err(no_such(format!("The entity {arn} cannot be found.")));
            }
            let id = ctx.uuid();
            acct.access_jobs.insert(id.clone(), t);
            json!({ "JobId": id })
        }
        "GetServiceLastAccessedDetails" | "GetServiceLastAccessedDetailsWithEntities" => {
            let id = required(input, "JobId")?;
            let at = acct
                .access_jobs
                .get(id)
                .ok_or_else(|| no_such(format!("Job {id} cannot be found.")))?;
            let mut out = json!({ "JobStatus": "COMPLETED", "JobCreationDate": at, "JobCompletionDate": at, "IsTruncated": false });
            if operation.ends_with("Entities") {
                out["EntityDetailsList"] = json!([]);
            } else {
                out["JobType"] = json!("SERVICE_LEVEL");
                out["ServicesLastAccessed"] = json!([]);
            }
            out
        }
        "ListPoliciesGrantingServiceAccess" => {
            let arn = required(input, "Arn")?;
            let (policies, _) = principal_policies(acct, arn)?;
            let services = strings(input.array_field("ServiceNamespaces"));
            json!({
                "PoliciesGrantingServiceAccess": services.iter().map(|ns| json!({
                    "ServiceNamespace": ns,
                    "Policies": policies.iter().filter(|p| {
                        p.doc.get("Statement").map(|s| s.to_string()).is_some_and(|s| s.contains(&format!("\"{ns}:")) || s.contains("\"*\""))
                    }).map(|p| json!({ "PolicyName": name_from_arn(&p.id), "PolicyType": if p.id.starts_with("arn:") { "MANAGED" } else { "INLINE" }, "PolicyArn": p.id.starts_with("arn:").then(|| p.id.clone()) })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
                "IsTruncated": false,
            })
        }
        _ => return Ok(None),
    }))
}

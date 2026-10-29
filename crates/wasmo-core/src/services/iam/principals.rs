use serde_json::{Value, json};

use super::{
    Account, Group, Handled, InstanceProfile, Role, User, arn, conflict, encode_doc, exists,
    invalid, no_such, now, page_value, path_of, required, tag_op, tag_values, tags_of, unique_id,
    validated,
};
use crate::codec::AwsError;
use crate::service::{Ctx, InputExt};

fn boundary_value(b: &Option<String>) -> Option<Value> {
    b.as_ref().map(|arn| {
        json!({ "PermissionsBoundaryType": "PermissionsBoundaryPolicy", "PermissionsBoundaryArn": arn })
    })
}

pub fn user_view(ctx: &Ctx<'_>, u: &User, tags: bool) -> Value {
    let mut v = json!({
        "Path": u.path,
        "UserName": u.name,
        "UserId": u.id,
        "Arn": arn(ctx, "user", &u.path, &u.name),
        "CreateDate": u.created,
    });
    if let Some(b) = boundary_value(&u.boundary) {
        v["PermissionsBoundary"] = b;
    }
    if tags && !u.tags.is_empty() {
        v["Tags"] = tag_values(&u.tags);
    }
    v
}

pub fn group_view(ctx: &Ctx<'_>, g: &Group) -> Value {
    json!({
        "Path": g.path,
        "GroupName": g.name,
        "GroupId": g.id,
        "Arn": arn(ctx, "group", &g.path, &g.name),
        "CreateDate": g.created,
    })
}

pub fn role_view(ctx: &Ctx<'_>, r: &Role, tags: bool) -> Value {
    let mut v = json!({
        "Path": r.path,
        "RoleName": r.name,
        "RoleId": r.id,
        "Arn": arn(ctx, "role", &r.path, &r.name),
        "CreateDate": r.created,
        "AssumeRolePolicyDocument": encode_doc(&r.trust),
        "MaxSessionDuration": r.max_session,
    });
    if let Some(d) = &r.description {
        v["Description"] = json!(d);
    }
    if let Some(b) = boundary_value(&r.boundary) {
        v["PermissionsBoundary"] = b;
    }
    if tags {
        if !r.tags.is_empty() {
            v["Tags"] = tag_values(&r.tags);
        }
        v["RoleLastUsed"] = json!({});
    }
    v
}

pub fn profile_view(ctx: &Ctx<'_>, acct: &Account, p: &InstanceProfile) -> Value {
    let mut v = json!({
        "Path": p.path,
        "InstanceProfileName": p.name,
        "InstanceProfileId": p.id,
        "Arn": arn(ctx, "instance-profile", &p.path, &p.name),
        "CreateDate": p.created,
        "Roles": p.roles.iter().filter_map(|r| acct.roles.get(r)).map(|r| role_view(ctx, r, false)).collect::<Vec<_>>(),
    });
    if !p.tags.is_empty() {
        v["Tags"] = tag_values(&p.tags);
    }
    v
}

fn by_path<'a, T>(
    items: impl Iterator<Item = &'a T>,
    input: &Value,
    path: impl Fn(&T) -> &str,
) -> Vec<&'a T>
where
    T: 'a,
{
    let prefix = input.str_field("PathPrefix").unwrap_or("/");
    items.filter(|x| path(x).starts_with(prefix)).collect()
}

fn inline_op(
    map: &mut std::collections::BTreeMap<String, String>,
    verb: &str,
    owner_key: &str,
    owner: &str,
    input: &Value,
) -> Result<Value, AwsError> {
    Ok(match verb {
        "Put" => {
            let name = required(input, "PolicyName")?.to_string();
            let doc = validated(input, "PolicyDocument")?;
            map.insert(name, doc);
            json!({})
        }
        "Get" => {
            let name = required(input, "PolicyName")?;
            let doc = map.get(name).ok_or_else(|| {
                no_such(format!(
                    "The {} policy with name {name} cannot be found.",
                    owner_key.trim_end_matches("Name").to_lowercase()
                ))
            })?;
            json!({ owner_key: owner, "PolicyName": name, "PolicyDocument": encode_doc(doc) })
        }
        "Delete" => {
            let name = required(input, "PolicyName")?;
            map.remove(name).ok_or_else(|| {
                no_such(format!(
                    "The {} policy with name {name} cannot be found.",
                    owner_key.trim_end_matches("Name").to_lowercase()
                ))
            })?;
            json!({})
        }
        _ => page_value(map.keys().map(|k| json!(k)).collect(), input, "PolicyNames"),
    })
}

fn attach_op(
    acct_policy: Result<super::ManagedPolicy, AwsError>,
    list: &mut Vec<String>,
    verb: &str,
    input: &Value,
) -> Result<Value, AwsError> {
    let arn = required(input, "PolicyArn")?.to_string();
    match verb {
        "Attach" => {
            acct_policy?;
            if !list.contains(&arn) {
                if list.len() >= 20 {
                    return Err(AwsError::new(
                        409,
                        "LimitExceeded",
                        "Cannot exceed quota for PoliciesPerRole: 20",
                    ));
                }
                list.push(arn);
            }
        }
        _ => {
            if !list.contains(&arn) {
                return Err(no_such(format!("Policy {arn} was not found.")));
            }
            list.retain(|a| *a != arn);
        }
    }
    Ok(json!({}))
}

fn attached_list(acct: &Account, list: &[String], input: &Value) -> Value {
    let prefix = input.str_field("PathPrefix").unwrap_or("/");
    page_value(
        list.iter()
            .filter_map(|a| acct.policy(a).ok())
            .filter(|p| p.path.starts_with(prefix))
            .map(|p| json!({ "PolicyName": p.name, "PolicyArn": p.arn }))
            .collect(),
        input,
        "AttachedPolicies",
    )
}

fn boundary_arn(acct: &Account, input: &Value) -> Result<String, AwsError> {
    let arn = required(input, "PermissionsBoundary")?.to_string();
    acct.policy(&arn)
        .map_err(|_| invalid(format!("ARN {arn} is not valid.")))?;
    Ok(arn)
}

pub fn users(acct: &mut Account, ctx: &mut Ctx<'_>, operation: &str, input: &Value) -> Handled {
    Ok(Some(match operation {
        "CreateUser" => {
            let name = required(input, "UserName")?.to_string();
            if acct.users.contains_key(&name) {
                return Err(exists(format!("User with name {name} already exists.")));
            }
            let u = User {
                id: unique_id(ctx, "AIDA"),
                path: path_of(input)?,
                created: now(ctx),
                tags: tags_of(input)?,
                boundary: input.str_field("PermissionsBoundary").map(str::to_string),
                name: name.clone(),
                ..Default::default()
            };
            let out = json!({ "User": user_view(ctx, &u, true) });
            acct.users.insert(name, u);
            out
        }
        "GetUser" => match input.str_field("UserName") {
            Some(name) => json!({ "User": user_view(ctx, acct.user(name)?, true) }),
            None => json!({ "User": {
                    "Path": "/",
                    "UserName": "root",
                    "UserId": ctx.account,
                    "Arn": format!("arn:aws:iam::{}:root", ctx.account),
                "CreateDate": 0.0,
            }}),
        },
        "ListUsers" => {
            let users = by_path(acct.users.values(), input, |u| &u.path);
            page_value(
                users.iter().map(|u| user_view(ctx, u, false)).collect(),
                input,
                "Users",
            )
        }
        "UpdateUser" => {
            let name = required(input, "UserName")?.to_string();
            let mut u = acct.user(&name)?.clone();
            if let Some(p) = input.str_field("NewPath") {
                u.path = p.to_string();
            }
            if let Some(n) = input.str_field("NewUserName").filter(|n| *n != name) {
                if acct.users.contains_key(n) {
                    return Err(exists(format!("User with name {n} already exists.")));
                }
                u.name = n.to_string();
                for g in acct.groups.values_mut() {
                    for m in g.members.iter_mut().filter(|m| **m == name) {
                        *m = n.to_string();
                    }
                }
                for k in acct.access_keys.iter_mut().filter(|k| k.user == name) {
                    k.user = n.to_string();
                }
            }
            acct.users.remove(&name);
            acct.users.insert(u.name.clone(), u);
            json!({})
        }
        "DeleteUser" => {
            let name = required(input, "UserName")?;
            let u = acct.user(name)?;
            if !u.inline.is_empty() || !u.attached.is_empty() {
                return Err(conflict(
                    "Cannot delete entity, must delete policies first.",
                ));
            }
            if u.login.is_some() {
                return Err(conflict(
                    "Cannot delete entity, must delete login profile first.",
                ));
            }
            if acct.access_keys.iter().any(|k| k.user == name) {
                return Err(conflict(
                    "Cannot delete entity, must delete access keys first.",
                ));
            }
            if !u.mfa.is_empty() {
                return Err(conflict(
                    "Cannot delete entity, must delete MFA device first.",
                ));
            }
            if acct
                .groups
                .values()
                .any(|g| g.members.iter().any(|m| m == name))
            {
                return Err(conflict(
                    "Cannot delete entity, must remove users from group first.",
                ));
            }
            acct.users.remove(name);
            json!({})
        }
        "TagUser" | "UntagUser" | "ListUserTags" => {
            let u = acct.user_mut(required(input, "UserName")?)?;
            tag_op(
                &mut u.tags,
                &operation[..operation.find("User").unwrap_or(0)].replace("List", ""),
                input,
            )?
        }
        "PutUserPolicy" | "GetUserPolicy" | "DeleteUserPolicy" | "ListUserPolicies" => {
            let name = required(input, "UserName")?.to_string();
            let u = acct.user_mut(&name)?;
            inline_op(
                &mut u.inline,
                &operation[..operation.find("User").unwrap_or(0)],
                "UserName",
                &name,
                input,
            )?
        }
        "AttachUserPolicy" | "DetachUserPolicy" => {
            let p = acct.policy(required(input, "PolicyArn")?);
            let u = acct.user_mut(required(input, "UserName")?)?;
            attach_op(p, &mut u.attached, &operation[..6], input)?
        }
        "ListAttachedUserPolicies" => {
            let u = acct.user(required(input, "UserName")?)?;
            attached_list(acct, &u.attached, input)
        }
        "PutUserPermissionsBoundary" => {
            let b = boundary_arn(acct, input)?;
            acct.user_mut(required(input, "UserName")?)?.boundary = Some(b);
            json!({})
        }
        "DeleteUserPermissionsBoundary" => {
            acct.user_mut(required(input, "UserName")?)?.boundary = None;
            json!({})
        }
        "CreateGroup" => {
            let name = required(input, "GroupName")?.to_string();
            if acct.groups.contains_key(&name) {
                return Err(exists(format!("Group with name {name} already exists.")));
            }
            let g = Group {
                id: unique_id(ctx, "AGPA"),
                path: path_of(input)?,
                created: now(ctx),
                name: name.clone(),
                ..Default::default()
            };
            let out = json!({ "Group": group_view(ctx, &g) });
            acct.groups.insert(name, g);
            out
        }
        "GetGroup" => {
            let g = acct.group_mut(required(input, "GroupName")?)?.clone();
            let users: Vec<Value> = g
                .members
                .iter()
                .filter_map(|m| acct.users.get(m))
                .map(|u| user_view(ctx, u, false))
                .collect();
            let mut out = page_value(users, input, "Users");
            out["Group"] = group_view(ctx, &g);
            out
        }
        "ListGroups" => {
            let groups = by_path(acct.groups.values(), input, |g| &g.path);
            page_value(
                groups.iter().map(|g| group_view(ctx, g)).collect(),
                input,
                "Groups",
            )
        }
        "UpdateGroup" => {
            let name = required(input, "GroupName")?.to_string();
            let mut g = acct.group_mut(&name)?.clone();
            if let Some(p) = input.str_field("NewPath") {
                g.path = p.to_string();
            }
            if let Some(n) = input.str_field("NewGroupName").filter(|n| *n != name) {
                if acct.groups.contains_key(n) {
                    return Err(exists(format!("Group with name {n} already exists.")));
                }
                g.name = n.to_string();
            }
            acct.groups.remove(&name);
            acct.groups.insert(g.name.clone(), g);
            json!({})
        }
        "DeleteGroup" => {
            let name = required(input, "GroupName")?;
            let g = acct.group_mut(name)?;
            if !g.members.is_empty() {
                return Err(conflict(
                    "Cannot delete entity, must remove users from group first.",
                ));
            }
            if !g.inline.is_empty() || !g.attached.is_empty() {
                return Err(conflict(
                    "Cannot delete entity, must delete policies first.",
                ));
            }
            acct.groups.remove(name);
            json!({})
        }
        "AddUserToGroup" | "RemoveUserFromGroup" => {
            let user = required(input, "UserName")?.to_string();
            acct.user(&user)?;
            let g = acct.group_mut(required(input, "GroupName")?)?;
            if operation == "AddUserToGroup" {
                if !g.members.contains(&user) {
                    g.members.push(user);
                }
            } else {
                if !g.members.contains(&user) {
                    return Err(no_such(format!(
                        "The user with name {user} cannot be found in the group."
                    )));
                }
                g.members.retain(|m| *m != user);
            }
            json!({})
        }
        "ListGroupsForUser" => {
            let user = required(input, "UserName")?;
            acct.user(user)?;
            page_value(
                acct.groups
                    .values()
                    .filter(|g| g.members.iter().any(|m| m == user))
                    .map(|g| group_view(ctx, g))
                    .collect(),
                input,
                "Groups",
            )
        }
        "PutGroupPolicy" | "GetGroupPolicy" | "DeleteGroupPolicy" | "ListGroupPolicies" => {
            let name = required(input, "GroupName")?.to_string();
            let g = acct.group_mut(&name)?;
            inline_op(
                &mut g.inline,
                &operation[..operation.find("Group").unwrap_or(0)],
                "GroupName",
                &name,
                input,
            )?
        }
        "AttachGroupPolicy" | "DetachGroupPolicy" => {
            let p = acct.policy(required(input, "PolicyArn")?);
            let g = acct.group_mut(required(input, "GroupName")?)?;
            attach_op(p, &mut g.attached, &operation[..6], input)?
        }
        "ListAttachedGroupPolicies" => {
            let g = acct.group_mut(required(input, "GroupName")?)?.clone();
            attached_list(acct, &g.attached, input)
        }
        _ => return Ok(None),
    }))
}

fn service_role_name(service: &str) -> String {
    let base = service.split('.').next().unwrap_or(service);
    let mut out = String::from("AWSServiceRoleFor");
    for part in base.split('-') {
        let mut c = part.chars();
        if let Some(f) = c.next() {
            out.extend(f.to_uppercase());
            out.push_str(c.as_str());
        }
    }
    out
}

pub fn roles(acct: &mut Account, ctx: &mut Ctx<'_>, operation: &str, input: &Value) -> Handled {
    Ok(Some(match operation {
        "CreateRole" => {
            let name = required(input, "RoleName")?.to_string();
            if acct.roles.contains_key(&name) {
                return Err(exists(format!("Role with name {name} already exists.")));
            }
            let max = input.i64_field("MaxSessionDuration").unwrap_or(3600);
            if !(3600..=43200).contains(&max) {
                return Err(AwsError::new(
                    400,
                    "ValidationError",
                    format!(
                        "1 validation error detected: Value '{max}' at 'maxSessionDuration' failed to satisfy constraint: Member must have value between 3600 and 43200"
                    ),
                ));
            }
            let r = Role {
                id: unique_id(ctx, "AROA"),
                path: path_of(input)?,
                created: now(ctx),
                trust: validated(input, "AssumeRolePolicyDocument")?,
                description: input.str_field("Description").map(str::to_string),
                max_session: max,
                tags: tags_of(input)?,
                boundary: input.str_field("PermissionsBoundary").map(str::to_string),
                name: name.clone(),
                ..Default::default()
            };
            let out = json!({ "Role": role_view(ctx, &r, false) });
            acct.roles.insert(name, r);
            out
        }
        "CreateServiceLinkedRole" => {
            let service = required(input, "AWSServiceName")?;
            let mut name = service_role_name(service);
            if let Some(s) = input.str_field("CustomSuffix") {
                name = format!("{name}_{s}");
            }
            if acct.roles.contains_key(&name) {
                return Err(invalid(format!(
                    "Service role name {name} has been taken in this account, please try a different suffix."
                )));
            }
            let trust = json!({
                "Version": "2012-10-17",
                "Statement": [{ "Effect": "Allow", "Principal": { "Service": [service] }, "Action": ["sts:AssumeRole"] }],
            })
            .to_string();
            let r = Role {
                id: unique_id(ctx, "AROA"),
                path: format!("/aws-service-role/{service}/"),
                created: now(ctx),
                trust,
                description: input.str_field("Description").map(str::to_string),
                max_session: 3600,
                name: name.clone(),
                ..Default::default()
            };
            let out = json!({ "Role": role_view(ctx, &r, false) });
            acct.roles.insert(name, r);
            out
        }
        "DeleteServiceLinkedRole" => {
            let name = required(input, "RoleName")?.to_string();
            acct.role(&name)?;
            acct.roles.remove(&name);
            let id = format!("task/aws-service-role/{}", ctx.uuid());
            acct.deletion_tasks
                .insert(id.clone(), "SUCCEEDED".to_string());
            json!({ "DeletionTaskId": id })
        }
        "GetServiceLinkedRoleDeletionStatus" => {
            let id = required(input, "DeletionTaskId")?;
            let status = acct
                .deletion_tasks
                .get(id)
                .ok_or_else(|| no_such(format!("Deletion task {id} not found.")))?;
            json!({ "Status": status })
        }
        "GetRole" => {
            json!({ "Role": role_view(ctx, acct.role(required(input, "RoleName")?)?, true) })
        }
        "ListRoles" => {
            let roles = by_path(acct.roles.values(), input, |r| &r.path);
            page_value(
                roles.iter().map(|r| role_view(ctx, r, false)).collect(),
                input,
                "Roles",
            )
        }
        "UpdateRole" | "UpdateRoleDescription" => {
            let r = acct.role_mut(required(input, "RoleName")?)?;
            if let Some(d) = input.get("Description").and_then(Value::as_str) {
                r.description = Some(d.to_string());
            }
            if let Some(m) = input.i64_field("MaxSessionDuration") {
                r.max_session = m;
            }
            if operation == "UpdateRoleDescription" {
                let r = r.clone();
                json!({ "Role": role_view(ctx, &r, true) })
            } else {
                json!({})
            }
        }
        "UpdateAssumeRolePolicy" => {
            let doc = validated(input, "PolicyDocument")?;
            acct.role_mut(required(input, "RoleName")?)?.trust = doc;
            json!({})
        }
        "DeleteRole" => {
            let name = required(input, "RoleName")?;
            let r = acct.role(name)?;
            if r.path.starts_with("/aws-service-role/") {
                return Err(AwsError::new(
                    400,
                    "UnmodifiableEntity",
                    format!(
                        "Cannot perform the operation on the protected role '{name}' - this role is only modifiable by AWS"
                    ),
                ));
            }
            if !r.inline.is_empty() || !r.attached.is_empty() {
                return Err(conflict(
                    "Cannot delete entity, must detach all policies first.",
                ));
            }
            if acct
                .profiles
                .values()
                .any(|p| p.roles.iter().any(|x| x == name))
            {
                return Err(conflict(
                    "Cannot delete entity, must remove roles from instance profile first.",
                ));
            }
            acct.roles.remove(name);
            json!({})
        }
        "TagRole" | "UntagRole" | "ListRoleTags" => {
            let r = acct.role_mut(required(input, "RoleName")?)?;
            tag_op(
                &mut r.tags,
                &operation[..operation.find("Role").unwrap_or(0)].replace("List", ""),
                input,
            )?
        }
        "PutRolePolicy" | "GetRolePolicy" | "DeleteRolePolicy" | "ListRolePolicies" => {
            let name = required(input, "RoleName")?.to_string();
            let r = acct.role_mut(&name)?;
            inline_op(
                &mut r.inline,
                &operation[..operation.find("Role").unwrap_or(0)],
                "RoleName",
                &name,
                input,
            )?
        }
        "AttachRolePolicy" | "DetachRolePolicy" => {
            let p = acct.policy(required(input, "PolicyArn")?);
            let r = acct.role_mut(required(input, "RoleName")?)?;
            attach_op(p, &mut r.attached, &operation[..6], input)?
        }
        "ListAttachedRolePolicies" => {
            let r = acct.role(required(input, "RoleName")?)?;
            attached_list(acct, &r.attached, input)
        }
        "PutRolePermissionsBoundary" => {
            let b = boundary_arn(acct, input)?;
            acct.role_mut(required(input, "RoleName")?)?.boundary = Some(b);
            json!({})
        }
        "DeleteRolePermissionsBoundary" => {
            acct.role_mut(required(input, "RoleName")?)?.boundary = None;
            json!({})
        }
        "CreateInstanceProfile" => {
            let name = required(input, "InstanceProfileName")?.to_string();
            if acct.profiles.contains_key(&name) {
                return Err(exists(format!("Instance Profile {name} already exists.")));
            }
            let p = InstanceProfile {
                id: unique_id(ctx, "AIPA"),
                path: path_of(input)?,
                created: now(ctx),
                tags: tags_of(input)?,
                name: name.clone(),
                roles: Vec::new(),
            };
            let out = json!({ "InstanceProfile": profile_view(ctx, acct, &p) });
            acct.profiles.insert(name, p);
            out
        }
        "GetInstanceProfile" => {
            let name = required(input, "InstanceProfileName")?;
            let p = acct
                .profiles
                .get(name)
                .ok_or_else(|| no_such(format!("Instance Profile {name} cannot be found.")))?;
            json!({ "InstanceProfile": profile_view(ctx, acct, p) })
        }
        "DeleteInstanceProfile" => {
            let name = required(input, "InstanceProfileName")?;
            let p = acct
                .profiles
                .get(name)
                .ok_or_else(|| no_such(format!("Instance Profile {name} cannot be found.")))?;
            if !p.roles.is_empty() {
                return Err(conflict(
                    "Cannot delete entity, must remove roles from instance profile first.",
                ));
            }
            acct.profiles.remove(name);
            json!({})
        }
        "ListInstanceProfiles" => {
            let ps = by_path(acct.profiles.values(), input, |p| &p.path);
            page_value(
                ps.iter().map(|p| profile_view(ctx, acct, p)).collect(),
                input,
                "InstanceProfiles",
            )
        }
        "ListInstanceProfilesForRole" => {
            let role = required(input, "RoleName")?;
            acct.role(role)?;
            page_value(
                acct.profiles
                    .values()
                    .filter(|p| p.roles.iter().any(|r| r == role))
                    .map(|p| profile_view(ctx, acct, p))
                    .collect(),
                input,
                "InstanceProfiles",
            )
        }
        "AddRoleToInstanceProfile" | "RemoveRoleFromInstanceProfile" => {
            let role = required(input, "RoleName")?.to_string();
            acct.role(&role)?;
            let name = required(input, "InstanceProfileName")?;
            let p = acct
                .profiles
                .get_mut(name)
                .ok_or_else(|| no_such(format!("Instance Profile {name} cannot be found.")))?;
            if operation.starts_with("Add") {
                if !p.roles.is_empty() {
                    return Err(AwsError::new(
                        409,
                        "LimitExceeded",
                        "Cannot exceed quota for InstanceSessionsPerInstanceProfile: 1",
                    ));
                }
                p.roles.push(role);
            } else {
                if !p.roles.contains(&role) {
                    return Err(no_such(format!(
                        "The role with name {role} cannot be found in the instance profile."
                    )));
                }
                p.roles.retain(|r| *r != role);
            }
            json!({})
        }
        "TagInstanceProfile" | "UntagInstanceProfile" | "ListInstanceProfileTags" => {
            let name = required(input, "InstanceProfileName")?;
            let p = acct
                .profiles
                .get_mut(name)
                .ok_or_else(|| no_such(format!("Instance Profile {name} cannot be found.")))?;
            let verb =
                operation[..operation.find("InstanceProfile").unwrap_or(0)].replace("List", "");
            tag_op(&mut p.tags, &verb, input)?
        }
        _ => return Ok(None),
    }))
}

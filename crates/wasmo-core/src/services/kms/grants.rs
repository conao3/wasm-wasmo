use serde_json::{Value, json};

use super::{Grant, Kms, err, opt_str, paginate, random_hex, required, secs, validation};
use crate::service::{Ctx, OpResult};

const OPERATIONS: &[&str] = &[
    "Decrypt",
    "Encrypt",
    "GenerateDataKey",
    "GenerateDataKeyWithoutPlaintext",
    "ReEncryptFrom",
    "ReEncryptTo",
    "Sign",
    "Verify",
    "GetPublicKey",
    "CreateGrant",
    "RetireGrant",
    "DescribeKey",
    "GenerateDataKeyPair",
    "GenerateDataKeyPairWithoutPlaintext",
    "GenerateMac",
    "VerifyMac",
    "DeriveSharedSecret",
];

fn entry(grant: &Grant, arn: &str, account: &str) -> Value {
    let mut v = json!({
        "KeyId": arn,
        "GrantId": grant.id,
        "CreationDate": grant.created,
        "IssuingAccount": format!("arn:aws:iam::{account}:root"),
        "Operations": grant.operations,
    });
    for (field, value) in [
        ("Name", &grant.name),
        ("GranteePrincipal", &grant.grantee),
        ("RetiringPrincipal", &grant.retiring),
    ] {
        if let Some(s) = value {
            v[field] = json!(s);
        }
    }
    if let Some(c) = &grant.constraints {
        v["Constraints"] = c.clone();
    }
    v
}

impl Kms {
    pub(super) fn create_grant(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let grantee = required(input, "GranteePrincipal")?.to_string();
        let operations = input
            .get("Operations")
            .and_then(Value::as_array)
            .filter(|ops| !ops.is_empty())
            .ok_or_else(|| validation("Operations must contain at least one operation."))?
            .clone();
        if let Some(bad) = operations
            .iter()
            .find(|op| !op.as_str().is_some_and(|s| OPERATIONS.contains(&s)))
        {
            return Err(validation(format!("Invalid grant operation {bad}")));
        }
        let constraints = input.get("Constraints").filter(|c| !c.is_null()).cloned();
        if let Some(c) = &constraints
            && c.get("EncryptionContextEquals").is_some()
            && c.get("EncryptionContextSubset").is_some()
        {
            return Err(validation(
                "Constraints accepts EncryptionContextEquals or EncryptionContextSubset, not both.",
            ));
        }
        let name = opt_str(input, "Name").map(str::to_string);
        let retiring = opt_str(input, "RetiringPrincipal").map(str::to_string);
        let key = self.input_key(ctx, input)?;
        key.mutable()?;
        let key_id = key.id.clone();
        let id = random_hex(ctx, 32);
        let token = random_hex(ctx, 64);
        let created = secs(ctx);
        let region = self.region(ctx);
        if let Some(g) = name.as_ref().and_then(|n| {
            region.grants.iter().find(|g| {
                g.key_id == key_id
                    && g.name.as_ref() == Some(n)
                    && g.grantee.as_ref() == Some(&grantee)
                    && g.operations == operations
                    && g.constraints == constraints
            })
        }) {
            return Ok(json!({"GrantToken": g.token, "GrantId": g.id}));
        }
        region.grants.push(Grant {
            id: id.clone(),
            token: token.clone(),
            key_id,
            name,
            grantee: Some(grantee),
            retiring,
            operations,
            constraints,
            created,
        });
        Ok(json!({"GrantToken": token, "GrantId": id}))
    }

    pub(super) fn list_grants(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let key = self.input_key(ctx, input)?;
        let (key_id, arn) = (key.id.clone(), key.arn.clone());
        let grant_id = opt_str(input, "GrantId");
        let grantee = opt_str(input, "GranteePrincipal");
        let account = ctx.account.clone();
        let items = self
            .region(ctx)
            .grants
            .iter()
            .filter(|g| g.key_id == key_id)
            .filter(|g| grant_id.is_none_or(|id| g.id == id))
            .filter(|g| grantee.is_none_or(|p| g.grantee.as_deref() == Some(p)))
            .map(|g| entry(g, &arn, &account))
            .collect();
        paginate(input, "Grants", items, 50)
    }

    pub(super) fn list_retirable_grants(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let principal = required(input, "RetiringPrincipal")?;
        let account = ctx.account.clone();
        let region = self.region(ctx);
        let items = region
            .grants
            .iter()
            .filter(|g| g.retiring.as_deref() == Some(principal))
            .map(|g| entry(g, &region.keys[&g.key_id].arn, &account))
            .collect();
        paginate(input, "Grants", items, 50)
    }

    pub(super) fn retire_grant(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        if let Some(token) = opt_str(input, "GrantToken") {
            let region = self.region(ctx);
            let before = region.grants.len();
            region.grants.retain(|g| g.token != token);
            if region.grants.len() == before {
                return Err(err(
                    "InvalidGrantTokenException",
                    "The grant token is invalid.",
                ));
            }
            return Ok(json!({}));
        }
        match (opt_str(input, "KeyId"), opt_str(input, "GrantId")) {
            (Some(_), Some(_)) => self.revoke_grant(ctx, input),
            _ => Err(validation(
                "RetireGrant requires GrantToken, or both KeyId and GrantId.",
            )),
        }
    }

    pub(super) fn revoke_grant(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let grant_id = required(input, "GrantId")?;
        let key_id = self.input_key(ctx, input)?.id.clone();
        let region = self.region(ctx);
        let before = region.grants.len();
        region
            .grants
            .retain(|g| !(g.key_id == key_id && g.id == grant_id));
        if region.grants.len() == before {
            return Err(err(
                "NotFoundException",
                format!("Grant ID {grant_id} not found"),
            ));
        }
        Ok(json!({}))
    }
}

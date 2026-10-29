use super::*;

impl Region {
    pub(super) fn create_secret(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let name = input
            .str_field("Name")
            .ok_or_else(|| invalid_param("You must specify a Name."))?;
        if !valid_name(name) {
            return Err(invalid_param(
                "Invalid name. Must be a valid name containing alphanumeric characters, or any of the following: -/_+=.@!",
            ));
        }
        let payload = payload(input)?;
        if let Some(existing) = self.secrets.get(name) {
            if existing.deleted.is_some() {
                return Err(invalid_request(
                    "You can't create this secret because a secret with this name is already scheduled for deletion.",
                ));
            }
            if let (Some(t), Some(p)) = (input.str_field("ClientRequestToken"), &payload)
                && existing
                    .versions
                    .iter()
                    .any(|v| v.id == t && &v.payload == p)
            {
                return Ok(json!({ "ARN": existing.arn, "Name": existing.name, "VersionId": t }));
            }
            return Err(exists(
                "The operation failed because the secret already exists.",
            ));
        }
        let now = secs(ctx);
        self.seq += 1;
        let resource = format!("secret:{name}-{}", arn_suffix(ctx));
        let mut secret = Secret {
            arn: ctx.arn("secretsmanager", &resource),
            name: name.to_string(),
            primary_region: ctx.region.clone(),
            description: opt_string(input, "Description"),
            kms_key_id: opt_string(input, "KmsKeyId"),
            secret_type: opt_string(input, "Type"),
            tags: Vec::new(),
            created: now,
            changed: now,
            accessed: None,
            deleted: None,
            versions: Vec::new(),
            rotation_enabled: false,
            rotation_lambda: None,
            rotation_rules: None,
            last_rotated: None,
            policy: None,
            replicas: Vec::new(),
            seq: self.seq,
        };
        merge_tags(&mut secret, input.array_field("Tags"));
        add_replicas(&mut secret, input.array_field("AddReplicaRegions"))?;
        let version_id = match payload {
            Some(p) => {
                let id = token(ctx, input);
                secret.add_version(id.clone(), p, &[CURRENT.to_string()], now)?;
                Some(id)
            }
            None => None,
        };
        let replication = (!secret.replicas.is_empty()).then(|| secret.replication_json());
        let out = json!({
            "ARN": secret.arn,
            "Name": secret.name,
            "VersionId": version_id,
            "ReplicationStatus": replication,
        });
        self.secrets.insert(name.to_string(), secret);
        Ok(out)
    }

    pub(super) fn get_secret_value(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let now = secs(ctx);
        let secret = self.get_mut(secret_id(input)?)?;
        secret.ensure_active()?;
        let idx = select_version(
            secret,
            input.str_field("VersionId"),
            input.str_field("VersionStage"),
        )?;
        secret.touch(idx, now);
        Ok(secret.value_json(idx))
    }

    pub(super) fn put_secret_value(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let now = secs(ctx);
        let id = token(ctx, input);
        let secret = self.get_mut(secret_id(input)?)?;
        secret.ensure_active()?;
        let payload = payload(input)?.ok_or_else(|| {
            invalid_request("You must provide either SecretString or SecretBinary.")
        })?;
        let stages = match input.get("VersionStages") {
            Some(Value::Array(a)) => strings(a),
            _ => vec![CURRENT.to_string()],
        };
        let stages = secret.add_version(id.clone(), payload, &stages, now)?;
        Ok(json!({
            "ARN": secret.arn,
            "Name": secret.name,
            "VersionId": id,
            "VersionStages": stages,
        }))
    }

    pub(super) fn update_secret(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let now = secs(ctx);
        let id = token(ctx, input);
        let secret = self.get_mut(secret_id(input)?)?;
        secret.ensure_active()?;
        let payload = payload(input)?;
        if let Some(d) = opt_string(input, "Description") {
            secret.description = Some(d);
        }
        if let Some(k) = opt_string(input, "KmsKeyId") {
            secret.kms_key_id = Some(k);
        }
        if let Some(t) = opt_string(input, "Type") {
            secret.secret_type = Some(t);
        }
        let version_id = match payload {
            Some(p) => {
                secret.add_version(id.clone(), p, &[CURRENT.to_string()], now)?;
                Some(id)
            }
            None => None,
        };
        secret.changed = now;
        Ok(json!({ "ARN": secret.arn, "Name": secret.name, "VersionId": version_id }))
    }

    pub(super) fn delete_secret(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let now = secs(ctx);
        let force = flag(input, "ForceDeleteWithoutRecovery") == Some(true);
        let window = input.i64_field("RecoveryWindowInDays");
        if force && window.is_some() {
            return Err(invalid_param(
                "You can't use ForceDeleteWithoutRecovery in conjunction with RecoveryWindowInDays.",
            ));
        }
        if let Some(w) = window.filter(|w| !(7..=30).contains(w)) {
            return Err(invalid_param(format!(
                "The recovery window must be between 7 and 30 days. You specified {w}."
            )));
        }
        let secret = self.get_mut(secret_id(input)?)?;
        if force {
            let (arn, name) = (secret.arn.clone(), secret.name.clone());
            self.secrets.remove(&name);
            return Ok(json!({ "ARN": arn, "Name": name, "DeletionDate": now }));
        }
        if secret.deleted.is_some() {
            return Err(invalid_request(
                "You can't perform this operation on the secret because it was already scheduled for deletion.",
            ));
        }
        let date = now + window.unwrap_or(30) as f64 * DAY;
        secret.deleted = Some(date);
        Ok(json!({ "ARN": secret.arn, "Name": secret.name, "DeletionDate": date }))
    }

    pub(super) fn restore_secret(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let now = secs(ctx);
        let secret = self.get_mut(secret_id(input)?)?;
        if secret.deleted.is_none() {
            return Err(invalid_request(
                "You can't restore this secret because it isn't scheduled for deletion.",
            ));
        }
        secret.deleted = None;
        secret.changed = now;
        Ok(json!({ "ARN": secret.arn, "Name": secret.name }))
    }

    pub(super) fn list_secrets(&self, input: &Value) -> OpResult {
        let limit = max_results(input, 100)?;
        let (sort_by, desc) = sort_options(input)?;
        let include_deleted = flag(input, "IncludePlannedDeletion") == Some(true);
        let filters = input.array_field("Filters");
        let mut list = Vec::new();
        for s in self.sorted(sort_by, desc) {
            if (include_deleted || s.deleted.is_none()) && s.matches(filters)? {
                list.push(s.describe(true));
            }
        }
        let (page, next) = paginate(list, input, limit)?;
        Ok(json!({ "SecretList": page, "NextToken": next }))
    }

    pub(super) fn batch_get_secret_value(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let now = secs(ctx);
        let ids = strings(input.array_field("SecretIdList"));
        let filters = input.array_field("Filters");
        if !ids.is_empty() && !filters.is_empty() {
            return Err(invalid_param(
                "Either SecretIdList or Filters must be specified, not both.",
            ));
        }
        let limit = max_results(input, 20)?;
        let (names, next) = if ids.is_empty() {
            let mut names = Vec::new();
            for s in self.sorted("created-date", false) {
                if s.deleted.is_none() && s.matches(filters)? {
                    names.push(s.name.clone());
                }
            }
            paginate(names, input, limit)?
        } else {
            (ids, None)
        };
        let mut values = Vec::new();
        let mut errors = Vec::new();
        for id in names {
            let result = self.get_mut(&id).and_then(|s| {
                s.ensure_active()?;
                let idx = select_version(s, None, None)?;
                s.touch(idx, now);
                Ok(s.value_json(idx))
            });
            match result {
                Ok(v) => values.push(v),
                Err(e) => errors.push(json!({
                    "SecretId": id,
                    "ErrorCode": e.code,
                    "Message": e.message,
                })),
            }
        }
        Ok(json!({ "SecretValues": values, "Errors": errors, "NextToken": next }))
    }

    pub(super) fn list_secret_version_ids(&self, input: &Value) -> OpResult {
        let limit = max_results(input, 100)?;
        let secret = self.get(secret_id(input)?)?;
        let include_deprecated = flag(input, "IncludeDeprecated") == Some(true);
        let versions: Vec<Value> = secret
            .versions
            .iter()
            .filter(|v| include_deprecated || !v.stages.is_empty())
            .map(|v| {
                json!({
                    "VersionId": v.id,
                    "VersionStages": v.stages,
                    "CreatedDate": v.created,
                    "LastAccessedDate": v.accessed,
                })
            })
            .collect();
        let (page, next) = paginate(versions, input, limit)?;
        Ok(json!({
            "ARN": secret.arn,
            "Name": secret.name,
            "Versions": page,
            "NextToken": next,
        }))
    }

    pub(super) fn update_secret_version_stage(
        &mut self,
        ctx: &mut Ctx<'_>,
        input: &Value,
    ) -> OpResult {
        let now = secs(ctx);
        let secret = self.get_mut(secret_id(input)?)?;
        secret.ensure_active()?;
        let stage = input
            .str_field("VersionStage")
            .ok_or_else(|| invalid_param("You must specify a VersionStage."))?;
        let holder = secret.holder(stage);
        let remove_from = match input.str_field("RemoveFromVersionId") {
            Some(id) => {
                let idx = secret.position(id).ok_or_else(|| {
                    invalid_param(format!("The version {id} doesn't exist for this secret."))
                })?;
                if holder != Some(idx) {
                    return Err(invalid_param(format!(
                        "The staging label {stage} is not attached to version {id}."
                    )));
                }
                Some(idx)
            }
            None => None,
        };
        match input.str_field("MoveToVersionId") {
            Some(id) => {
                let to = secret.position(id).ok_or_else(|| {
                    err(
                        "ResourceNotFoundException",
                        format!("Secrets Manager can't find the specified secret version: {id}"),
                    )
                })?;
                if let Some(h) = holder.filter(|h| *h != to && remove_from != Some(*h)) {
                    return Err(invalid_param(format!(
                        "The staging label {stage} is currently attached to version {}. You must specify RemoveFromVersionId to remove it.",
                        secret.versions[h].id
                    )));
                }
                secret.attach(stage, to);
            }
            None => {
                let from = remove_from.ok_or_else(|| {
                    invalid_param("You must specify MoveToVersionId or RemoveFromVersionId.")
                })?;
                if stage == CURRENT {
                    return Err(invalid_param(
                        "You can't remove the AWSCURRENT staging label without moving it to another version.",
                    ));
                }
                secret.versions[from].stages.retain(|s| s != stage);
            }
        }
        secret.changed = now;
        Ok(json!({ "ARN": secret.arn, "Name": secret.name }))
    }

    pub(super) fn rotate_secret(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let now = secs(ctx);
        let id = token(ctx, input);
        let secret = self.get_mut(secret_id(input)?)?;
        secret.ensure_active()?;
        if let Some(rules) = input.get("RotationRules").filter(|r| r.is_object()) {
            if let Some(days) = rules
                .i64_field("AutomaticallyAfterDays")
                .filter(|d| !(1..=1000).contains(d))
            {
                return Err(invalid_param(format!(
                    "The rotation interval must be between 1 and 1000 days. You specified {days}."
                )));
            }
            secret.rotation_rules = Some(rules.clone());
        }
        if let Some(lambda) = opt_string(input, "RotationLambdaARN") {
            secret.rotation_lambda = Some(lambda);
        }
        secret.rotation_enabled = true;
        secret.changed = now;
        if flag(input, "RotateImmediately") == Some(false) {
            return Ok(json!({ "ARN": secret.arn, "Name": secret.name }));
        }
        if let Some(existing) = secret.position(&id) {
            if secret.holder(CURRENT) == Some(existing) {
                return Ok(json!({ "ARN": secret.arn, "Name": secret.name, "VersionId": id }));
            }
            return Err(exists(
                "A resource with the ID you requested already exists.",
            ));
        }
        let current = secret
            .holder(CURRENT)
            .ok_or_else(|| invalid_request("The secret has no AWSCURRENT version to rotate."))?;
        if secret.holder(PENDING).is_some_and(|p| p != current) {
            return Err(invalid_request(
                "A previous rotation isn't complete. That rotation will be reattempted.",
            ));
        }
        secret.detach(PENDING);
        let payload = secret.versions[current].payload.clone();
        secret.add_version(id.clone(), payload, &[CURRENT.to_string()], now)?;
        secret.last_rotated = Some(now);
        Ok(json!({ "ARN": secret.arn, "Name": secret.name, "VersionId": id }))
    }

    pub(super) fn cancel_rotate_secret(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let now = secs(ctx);
        let secret = self.get_mut(secret_id(input)?)?;
        secret.ensure_active()?;
        let version = secret
            .holder(PENDING)
            .or_else(|| secret.holder(CURRENT))
            .map(|i| secret.versions[i].id.clone());
        secret.detach(PENDING);
        secret.rotation_enabled = false;
        secret.changed = now;
        Ok(json!({ "ARN": secret.arn, "Name": secret.name, "VersionId": version }))
    }

    pub(super) fn tag_resource(&mut self, input: &Value) -> OpResult {
        let secret = self.get_mut(secret_id(input)?)?;
        merge_tags(secret, input.array_field("Tags"));
        Ok(json!({}))
    }

    pub(super) fn untag_resource(&mut self, input: &Value) -> OpResult {
        let secret = self.get_mut(secret_id(input)?)?;
        let keys = strings(input.array_field("TagKeys"));
        secret.tags.retain(|(k, _)| !keys.contains(k));
        Ok(json!({}))
    }

    pub(super) fn put_resource_policy(&mut self, input: &Value) -> OpResult {
        let policy = policy_document(input)?;
        if flag(input, "BlockPublicPolicy") == Some(true) && is_public(&policy) {
            return Err(err(
                "PublicPolicyException",
                "You can't add the resource policy because it allows broad access to the secret.",
            ));
        }
        let secret = self.get_mut(secret_id(input)?)?;
        secret.policy = opt_string(input, "ResourcePolicy");
        Ok(json!({ "ARN": secret.arn, "Name": secret.name }))
    }

    pub(super) fn validate_resource_policy(&self, input: &Value) -> OpResult {
        policy_document(input)?;
        if let Some(id) = input.str_field("SecretId") {
            self.get(id)?;
        }
        Ok(json!({ "PolicyValidationPassed": true, "ValidationErrors": [] }))
    }
}

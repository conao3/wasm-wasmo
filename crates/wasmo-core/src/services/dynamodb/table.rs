use std::collections::BTreeSet;

use serde_json::{Value, json};

use crate::codec::AwsError;
use crate::codec::time::format_iso8601;
use crate::service::{Ctx, InputExt, OpResult};

use super::attr::{Attr, Item, item_size};
use super::{
    Index, KeySchema, KeyVal, Pk, Region, Table, err, require_table_name, table_arn, table_name,
    validation,
};

fn invalid(message: impl std::fmt::Display) -> AwsError {
    validation(format!(
        "One or more parameter values were invalid: {message}"
    ))
}

fn constraint(value: impl std::fmt::Display, field: &str, rule: &str) -> AwsError {
    validation(format!(
        "1 validation error detected: Value '{value}' at '{field}' failed to satisfy constraint: {rule}"
    ))
}

fn parse_key_schema(v: Option<&Value>, field: &str) -> Result<KeySchema, AwsError> {
    let elems = v.and_then(Value::as_array).ok_or_else(|| {
        validation(format!(
            "1 validation error detected: Value null at '{field}' failed to satisfy constraint: Member must not be null"
        ))
    })?;
    if elems.is_empty() || elems.len() > 2 {
        return Err(constraint(
            format!("{elems:?}"),
            field,
            "Member must have length less than or equal to 2",
        ));
    }
    let part = |i: usize| {
        let e = &elems[i];
        (
            e.str_field("AttributeName").unwrap_or_default().to_string(),
            e.str_field("KeyType").unwrap_or_default(),
        )
    };
    let (hash, kt) = part(0);
    if kt != "HASH" {
        return Err(validation(
            "Invalid KeySchema: The first KeySchemaElement is not a HASH key type",
        ));
    }
    let range = if elems.len() == 2 {
        let (r, kt) = part(1);
        if kt != "RANGE" {
            return Err(validation(
                "Invalid KeySchema: The second KeySchemaElement is not a RANGE key type",
            ));
        }
        if r == hash {
            return Err(invalid(
                "Both the Hash Key and the Range Key element in the KeySchema have the same name",
            ));
        }
        Some(r)
    } else {
        None
    };
    Ok(KeySchema { hash, range })
}

fn parse_throughput(v: Option<&Value>) -> Option<(i64, i64)> {
    let v = v?;
    Some((
        v.i64_field("ReadCapacityUnits")?,
        v.i64_field("WriteCapacityUnits")?,
    ))
}

fn parse_index(
    v: &Value,
    global: bool,
    provisioned: bool,
    status: &str,
) -> Result<Index, AwsError> {
    let name = v.str_field("IndexName").unwrap_or_default().to_string();
    if name.len() < 3 || name.len() > 255 {
        return Err(constraint(
            &name,
            "indexName",
            "Member must have length greater than or equal to 3",
        ));
    }
    let key = parse_key_schema(v.get("KeySchema"), "keySchema")?;
    let projection = v.get("Projection").cloned().unwrap_or(Value::Null);
    let projection_type = projection
        .str_field("ProjectionType")
        .ok_or_else(|| invalid(format!("Unknown ProjectionType: null for index: {name}")))?
        .to_string();
    let non_key: Vec<String> = projection
        .array_field("NonKeyAttributes")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    match projection_type.as_str() {
        "ALL" | "KEYS_ONLY" if !non_key.is_empty() => {
            return Err(invalid(format!(
                "ProjectionType is {projection_type}, but NonKeyAttributes is specified"
            )));
        }
        "INCLUDE" if non_key.is_empty() => {
            return Err(invalid(
                "NonKeyAttributes must be specified when ProjectionType is INCLUDE",
            ));
        }
        "ALL" | "KEYS_ONLY" | "INCLUDE" => {}
        other => {
            return Err(invalid(format!(
                "Unknown ProjectionType: {other} for index: {name}"
            )));
        }
    }
    let throughput = parse_throughput(v.get("ProvisionedThroughput"));
    if global && provisioned && throughput.is_none() {
        return Err(invalid(format!(
            "ProvisionedThroughput should not be null for index: {name}"
        )));
    }
    Ok(Index {
        name,
        key,
        projection_type,
        non_key,
        throughput: if provisioned { throughput } else { None },
        status: status.to_string(),
        global,
    })
}

fn parse_attr_defs(input: &Value) -> Result<Vec<(String, String)>, AwsError> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (i, d) in input.array_field("AttributeDefinitions").iter().enumerate() {
        let name = d.str_field("AttributeName").unwrap_or_default().to_string();
        let ty = d.str_field("AttributeType").unwrap_or_default().to_string();
        if !matches!(ty.as_str(), "S" | "N" | "B") {
            return Err(constraint(
                &ty,
                &format!("attributeDefinitions.{}.member.attributeType", i + 1),
                "Member must satisfy enum value set: [B, N, S]",
            ));
        }
        if out.iter().any(|(n, _)| *n == name) {
            return Err(invalid("Cannot have two attributes with the same name"));
        }
        out.push((name, ty));
    }
    Ok(out)
}

fn check_defined(defs: &[(String, String)], keys: &BTreeSet<&String>) -> Result<(), AwsError> {
    let missing: Vec<&str> = keys
        .iter()
        .filter(|k| !defs.iter().any(|(n, _)| n == **k))
        .map(|k| k.as_str())
        .collect();
    if !missing.is_empty() {
        let defined: Vec<&str> = defs.iter().map(|(n, _)| n.as_str()).collect();
        return Err(invalid(format!(
            "Some index key attributes are not defined in AttributeDefinitions. Keys: [{}], AttributeDefinitions: [{}]",
            missing.join(", "),
            defined.join(", ")
        )));
    }
    Ok(())
}

fn validate_table_name(name: &str) -> Result<(), AwsError> {
    if name.len() < 3 || name.len() > 255 {
        return Err(constraint(
            name,
            "tableName",
            "Member must have length greater than or equal to 3",
        ));
    }
    if !name
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
    {
        return Err(constraint(
            name,
            "tableName",
            "Member must satisfy regular expression pattern: [a-zA-Z0-9_.-]+",
        ));
    }
    Ok(())
}

fn stream_label(now_ms: f64) -> String {
    format_iso8601(now_ms.floor() / 1000.0)
        .trim_end_matches('Z')
        .to_string()
}

fn parse_stream(
    spec: Option<&Value>,
    now_ms: f64,
) -> Result<Option<(bool, String, String)>, AwsError> {
    let Some(spec) = spec else {
        return Ok(None);
    };
    if spec.get("StreamEnabled").and_then(Value::as_bool) != Some(true) {
        return Ok(None);
    }
    let view = spec.str_field("StreamViewType").ok_or_else(|| {
        validation("StreamSpecification: StreamViewType is required when StreamEnabled is true")
    })?;
    Ok(Some((true, view.to_string(), stream_label(now_ms))))
}

pub fn create(region: &mut Region, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let name = require_table_name(input)?.to_string();
    validate_table_name(&name)?;
    let attr_defs = parse_attr_defs(input)?;
    let key = parse_key_schema(input.get("KeySchema"), "keySchema")?;
    let billing = input
        .str_field("BillingMode")
        .unwrap_or("PROVISIONED")
        .to_string();
    let provisioned = billing == "PROVISIONED";
    let throughput = parse_throughput(input.get("ProvisionedThroughput"));
    if provisioned && throughput.is_none() {
        return Err(invalid(
            "ReadCapacityUnits and WriteCapacityUnits must both be specified when BillingMode is PROVISIONED",
        ));
    }
    if !provisioned && throughput.is_some_and(|(r, w)| r > 0 || w > 0) {
        return Err(invalid(
            "Neither ReadCapacityUnits nor WriteCapacityUnits can be specified when BillingMode is PAY_PER_REQUEST",
        ));
    }
    let mut names = BTreeSet::new();
    let mut gsis = Vec::new();
    for g in input.array_field("GlobalSecondaryIndexes") {
        let idx = parse_index(g, true, provisioned, "ACTIVE")?;
        if !names.insert(idx.name.clone()) {
            return Err(invalid(format!("Duplicate index name: {}", idx.name)));
        }
        gsis.push(idx);
    }
    let mut lsis = Vec::new();
    for l in input.array_field("LocalSecondaryIndexes") {
        let idx = parse_index(l, false, provisioned, "ACTIVE")?;
        if !names.insert(idx.name.clone()) {
            return Err(invalid(format!("Duplicate index name: {}", idx.name)));
        }
        if key.range.is_none() {
            return Err(invalid(
                "Table KeySchema does not have a range key, which is required when specifying a LocalSecondaryIndex",
            ));
        }
        if idx.key.hash != key.hash {
            return Err(invalid(format!(
                "Index KeySchema does not have the same leading hash key as table KeySchema for index: {}. index hash key: {}, table hash key: {}",
                idx.name, idx.key.hash, key.hash
            )));
        }
        if idx.key.range.is_none() {
            return Err(invalid(format!(
                "Index KeySchema does not have a range key for index: {}",
                idx.name
            )));
        }
        lsis.push(idx);
    }
    if gsis.len() > 20 {
        return Err(validation(
            "One or more parameter values were invalid: GlobalSecondaryIndex count exceeds the per-table limit of 20",
        ));
    }
    if lsis.len() > 5 {
        return Err(validation(
            "One or more parameter values were invalid: Number of LocalSecondaryIndexes exceeds per-table limit of 5",
        ));
    }
    let used: BTreeSet<&String> = key
        .names()
        .chain(gsis.iter().chain(&lsis).flat_map(|i| i.key.names()))
        .collect();
    check_defined(&attr_defs, &used)?;
    if used.len() != attr_defs.len() {
        return Err(invalid(
            "Number of attributes in KeySchema does not exactly match number of attributes defined in AttributeDefinitions",
        ));
    }
    if region.tables.contains_key(&name) {
        return Err(err(
            "ResourceInUseException",
            format!("Table already exists: {name}"),
        ));
    }
    let now = ctx.now_ms();
    let table = Table {
        name: name.clone(),
        id: ctx.uuid(),
        created: (now / 1000.0).floor(),
        status: "CREATING".into(),
        attr_defs,
        key,
        gsis,
        lsis,
        billing,
        billing_changed: (now / 1000.0).floor(),
        throughput: throughput.filter(|_| provisioned).unwrap_or((0, 0)),
        items: Default::default(),
        tags: tags_of(input),
        ttl: None,
        stream: parse_stream(input.get("StreamSpecification"), now)?,
        pitr: false,
        deletion_protection: input
            .get("DeletionProtectionEnabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        table_class: input.str_field("TableClass").map(str::to_string),
        sse: input
            .get("SSESpecification")
            .filter(|s| s.get("Enabled").and_then(Value::as_bool) == Some(true))
            .cloned(),
        restore_summary: None,
    };
    let out = json!({ "TableDescription": description(ctx, &table) });
    region.tables.insert(name, table);
    Ok(out)
}

fn tags_of(input: &Value) -> Vec<(String, String)> {
    input
        .array_field("Tags")
        .iter()
        .map(|t| {
            (
                t.str_field("Key").unwrap_or_default().to_string(),
                t.get("Value")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect()
}

fn throughput_value(t: (i64, i64)) -> Value {
    json!({
        "NumberOfDecreasesToday": 0,
        "ReadCapacityUnits": t.0,
        "WriteCapacityUnits": t.1,
    })
}

pub fn projection_value(i: &Index) -> Value {
    let mut p = json!({ "ProjectionType": i.projection_type });
    if !i.non_key.is_empty() {
        p["NonKeyAttributes"] = json!(i.non_key);
    }
    p
}

pub fn description(ctx: &Ctx<'_>, t: &Table) -> Value {
    let arn = table_arn(ctx, &t.name);
    let mut d = json!({
        "TableName": t.name,
        "TableId": t.id,
        "TableArn": arn,
        "TableStatus": t.status,
        "CreationDateTime": t.created,
        "AttributeDefinitions": t.attr_defs.iter().map(|(n, ty)| json!({ "AttributeName": n, "AttributeType": ty })).collect::<Vec<_>>(),
        "KeySchema": t.key.to_value(),
        "ProvisionedThroughput": throughput_value(t.throughput),
        "ItemCount": t.items.len(),
        "TableSizeBytes": t.items.values().map(item_size).sum::<usize>(),
        "DeletionProtectionEnabled": t.deletion_protection,
    });
    if t.billing == "PAY_PER_REQUEST" {
        d["BillingModeSummary"] = json!({
            "BillingMode": "PAY_PER_REQUEST",
            "LastUpdateToPayPerRequestDateTime": t.billing_changed,
        });
    }
    let index_value = |i: &Index| {
        let members: Vec<&Item> = t.items.values().filter(|it| t.in_index(i, it)).collect();
        let mut v = json!({
            "IndexName": i.name,
            "KeySchema": i.key.to_value(),
            "Projection": projection_value(i),
            "IndexSizeBytes": members.iter().map(|it| item_size(it)).sum::<usize>(),
            "ItemCount": members.len(),
            "IndexArn": format!("{arn}/index/{}", i.name),
        });
        if i.global {
            v["IndexStatus"] = json!(i.status);
            v["ProvisionedThroughput"] = throughput_value(i.throughput.unwrap_or((0, 0)));
            if i.status == "CREATING" {
                v["Backfilling"] = json!(false);
            }
        }
        v
    };
    if !t.gsis.is_empty() {
        d["GlobalSecondaryIndexes"] = t.gsis.iter().map(index_value).collect();
    }
    if !t.lsis.is_empty() {
        d["LocalSecondaryIndexes"] = t.lsis.iter().map(index_value).collect();
    }
    if let Some((enabled, view, label)) = &t.stream {
        d["StreamSpecification"] = if *enabled {
            json!({ "StreamEnabled": true, "StreamViewType": view })
        } else {
            json!({ "StreamEnabled": false })
        };
        d["LatestStreamLabel"] = json!(label);
        d["LatestStreamArn"] = json!(format!("{arn}/stream/{label}"));
    }
    if let Some(sse) = &t.sse {
        d["SSEDescription"] = json!({
            "Status": "ENABLED",
            "SSEType": sse.str_field("SSEType").unwrap_or("KMS"),
            "KMSMasterKeyArn": sse.str_field("KMSMasterKeyId").map_or_else(
                || format!("arn:aws:kms:{}:{}:key/aws/dynamodb", ctx.region, ctx.account),
                str::to_string,
            ),
        });
    }
    if let Some(class) = &t.table_class {
        d["TableClassSummary"] = json!({ "TableClass": class });
    }
    if let Some(r) = &t.restore_summary {
        d["RestoreSummary"] = r.clone();
    }
    d
}

pub fn describe(region: &mut Region, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let name = require_table_name(input)?;
    let t = region.tables.get(table_name(name)).ok_or_else(|| {
        err(
            "ResourceNotFoundException",
            format!(
                "Requested resource not found: Table: {} not found",
                table_name(name)
            ),
        )
    })?;
    Ok(json!({ "Table": description(ctx, t) }))
}

pub fn list(region: &mut Region, input: &Value) -> OpResult {
    let limit = match input.i64_field("Limit") {
        Some(l) if !(1..=100).contains(&l) => {
            return Err(constraint(
                l,
                "limit",
                "Member must have value less than or equal to 100",
            ));
        }
        Some(l) => l as usize,
        None => 100,
    };
    let start = input.str_field("ExclusiveStartTableName");
    let names: Vec<&String> = region
        .tables
        .keys()
        .filter(|n| start.is_none_or(|s| n.as_str() > s))
        .collect();
    let page: Vec<&String> = names.iter().take(limit).copied().collect();
    let mut out = json!({ "TableNames": page });
    if names.len() > limit {
        out["LastEvaluatedTableName"] = json!(page.last());
    }
    Ok(out)
}

pub fn update(region: &mut Region, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let name = require_table_name(input)?;
    let now = ctx.now_ms();
    let t = region.tables.get_mut(table_name(name)).ok_or_else(|| {
        err(
            "ResourceNotFoundException",
            format!(
                "Requested resource not found: Table: {} not found",
                table_name(name)
            ),
        )
    })?;
    let mut next = t.clone();
    let mut changed = false;
    for (n, ty) in parse_attr_defs(input)? {
        match next.attr_defs.iter_mut().find(|(e, _)| *e == n) {
            Some(e) => e.1 = ty,
            None => next.attr_defs.push((n, ty)),
        }
    }
    if let Some(billing) = input.str_field("BillingMode") {
        if billing != next.billing {
            next.billing = billing.to_string();
            next.billing_changed = (now / 1000.0).floor();
            if billing == "PAY_PER_REQUEST" {
                next.throughput = (0, 0);
                for g in &mut next.gsis {
                    g.throughput = None;
                }
            }
        }
        changed = true;
    }
    if let Some(tp) = parse_throughput(input.get("ProvisionedThroughput")) {
        if next.billing == "PAY_PER_REQUEST" {
            return Err(invalid(
                "Neither ReadCapacityUnits nor WriteCapacityUnits can be specified when BillingMode is PAY_PER_REQUEST",
            ));
        }
        if tp == next.throughput && !changed && input.get("GlobalSecondaryIndexUpdates").is_none() {
            return Err(validation(format!(
                "The provisioned throughput for the table will not change. The requested value equals the current value. Current ReadCapacityUnits provisioned for the table: {}. Requested ReadCapacityUnits: {}. Current WriteCapacityUnits provisioned for the table: {}. Requested WriteCapacityUnits: {}. Refer to the Amazon DynamoDB Developer Guide for current limits and how to request higher limits.",
                next.throughput.0, tp.0, next.throughput.1, tp.1
            )));
        }
        next.throughput = tp;
        changed = true;
    } else if next.billing == "PROVISIONED" && t.billing == "PAY_PER_REQUEST" {
        return Err(invalid(
            "ProvisionedThroughput must be specified when BillingMode is PROVISIONED",
        ));
    }
    let provisioned = next.billing == "PROVISIONED";
    for u in input.array_field("GlobalSecondaryIndexUpdates") {
        changed = true;
        if let Some(c) = u.get("Create") {
            let idx = parse_index(c, true, provisioned, "CREATING")?;
            if next
                .gsis
                .iter()
                .chain(&next.lsis)
                .any(|i| i.name == idx.name)
            {
                return Err(invalid(format!(
                    "Index with name {} already exists",
                    idx.name
                )));
            }
            next.gsis.push(idx);
        } else if let Some(d) = u.get("Delete") {
            let iname = d.str_field("IndexName").unwrap_or_default();
            let g = next
                .gsis
                .iter_mut()
                .find(|i| i.name == iname)
                .ok_or_else(|| {
                    err(
                        "ResourceNotFoundException",
                        format!("Requested resource not found: Index: {iname} not found"),
                    )
                })?;
            g.status = "DELETING".into();
        } else if let Some(up) = u.get("Update") {
            let iname = up.str_field("IndexName").unwrap_or_default();
            let g = next
                .gsis
                .iter_mut()
                .find(|i| i.name == iname)
                .ok_or_else(|| {
                    err(
                        "ResourceNotFoundException",
                        format!("Requested resource not found: Index: {iname} not found"),
                    )
                })?;
            if let Some(tp) = parse_throughput(up.get("ProvisionedThroughput")) {
                g.throughput = Some(tp);
                g.status = "UPDATING".into();
            }
        }
    }
    if let Some(spec) = input.get("StreamSpecification") {
        changed = true;
        let enable = spec.get("StreamEnabled").and_then(Value::as_bool) == Some(true);
        match (&next.stream, enable) {
            (Some((true, _, _)), true) => {
                return Err(validation(format!(
                    "Table already has an enabled stream: {}",
                    table_arn(ctx, &next.name)
                )));
            }
            (_, true) => next.stream = parse_stream(Some(spec), now)?,
            (Some((true, view, label)), false) => {
                next.stream = Some((false, view.clone(), label.clone()));
            }
            (_, false) => {
                return Err(validation("Table already has no stream enabled"));
            }
        }
    }
    if let Some(dp) = input
        .get("DeletionProtectionEnabled")
        .and_then(Value::as_bool)
    {
        next.deletion_protection = dp;
        changed = true;
    }
    if let Some(class) = input.str_field("TableClass") {
        next.table_class = Some(class.to_string());
        changed = true;
    }
    if let Some(sse) = input.get("SSESpecification") {
        next.sse = (sse.get("Enabled").and_then(Value::as_bool) == Some(true)).then(|| sse.clone());
        changed = true;
    }
    if !changed {
        return Err(validation(
            "At least one of ProvisionedThroughput, BillingMode, UpdateStreamEnabled, GlobalSecondaryIndexUpdates or SSESpecification or ReplicaUpdates is required",
        ));
    }
    let used: BTreeSet<&String> = next
        .key
        .names()
        .chain(
            next.gsis
                .iter()
                .chain(&next.lsis)
                .flat_map(|i| i.key.names()),
        )
        .collect();
    check_defined(&next.attr_defs, &used)?;
    next.attr_defs.retain(|(n, _)| used.contains(n));
    next.status = "UPDATING".into();
    let out = json!({ "TableDescription": description(ctx, &next) });
    *t = next;
    Ok(out)
}

pub fn delete(region: &mut Region, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let name = table_name(require_table_name(input)?).to_string();
    let t = region.tables.get(&name).ok_or_else(|| {
        err(
            "ResourceNotFoundException",
            format!("Requested resource not found: Table: {name} not found"),
        )
    })?;
    if t.deletion_protection {
        return Err(validation(
            "Resource cannot be deleted as it is currently protected against deletion. Disable deletion protection first.",
        ));
    }
    let mut t = region.tables.remove(&name).expect("table exists");
    region.policies.remove(&table_arn(ctx, &name));
    t.status = "DELETING".into();
    Ok(json!({ "TableDescription": description(ctx, &t) }))
}

pub fn update_ttl(region: &mut Region, input: &Value) -> OpResult {
    let t = region.table_mut(require_table_name(input)?)?;
    let spec = input
        .get("TimeToLiveSpecification")
        .ok_or_else(|| validation("TimeToLiveSpecification is required"))?;
    let enabled = spec
        .get("Enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let attr = spec
        .str_field("AttributeName")
        .unwrap_or_default()
        .to_string();
    if attr.is_empty() {
        return Err(validation(
            "1 validation error detected: Value '' at 'timeToLiveSpecification.attributeName' failed to satisfy constraint: Member must have length greater than or equal to 1",
        ));
    }
    match (&t.ttl, enabled) {
        (Some(_), true) => return Err(validation("TimeToLive is already enabled")),
        (None, false) => return Err(validation("TimeToLive is already disabled")),
        (Some(cur), false) if *cur != attr => {
            return Err(validation(format!(
                "TimeToLive is active on a different AttributeName: current AttributeName is {cur}"
            )));
        }
        _ => {}
    }
    t.ttl = enabled.then(|| attr.clone());
    Ok(json!({ "TimeToLiveSpecification": { "Enabled": enabled, "AttributeName": attr } }))
}

pub fn describe_ttl(region: &mut Region, input: &Value) -> OpResult {
    let t = region.table(require_table_name(input)?)?;
    Ok(json!({
        "TimeToLiveDescription": match &t.ttl {
            Some(a) => json!({ "TimeToLiveStatus": "ENABLED", "AttributeName": a }),
            None => json!({ "TimeToLiveStatus": "DISABLED" }),
        }
    }))
}

fn resource_table<'a>(region: &'a mut Region, input: &Value) -> Result<&'a mut Table, AwsError> {
    let arn = input.str_field("ResourceArn").ok_or_else(|| {
        validation(
            "1 validation error detected: Value null at 'resourceArn' failed to satisfy constraint: Member must not be null",
        )
    })?;
    if !arn.starts_with("arn:") {
        return Err(validation(format!(
            "Invalid TableArn: Invalid ResourceArn provided as input {arn}"
        )));
    }
    let name = table_name(arn).to_string();
    region.tables.get_mut(&name).ok_or_else(|| {
        err(
            "ResourceNotFoundException",
            format!("Requested resource not found: ResourcArn: {arn} not found"),
        )
    })
}

pub fn tag(region: &mut Region, input: &Value) -> OpResult {
    let tags = tags_of(input);
    let t = resource_table(region, input)?;
    for (k, v) in tags {
        match t.tags.iter_mut().find(|(e, _)| *e == k) {
            Some(e) => e.1 = v,
            None => t.tags.push((k, v)),
        }
    }
    if t.tags.len() > 50 {
        return Err(err(
            "LimitExceededException",
            "The number of tags on the resource exceeds the limit of 50",
        ));
    }
    Ok(json!({}))
}

pub fn untag(region: &mut Region, input: &Value) -> OpResult {
    let keys: Vec<&str> = input
        .array_field("TagKeys")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let t = resource_table(region, input)?;
    t.tags.retain(|(k, _)| !keys.contains(&k.as_str()));
    Ok(json!({}))
}

pub fn list_tags(region: &mut Region, input: &Value) -> OpResult {
    let t = resource_table(region, input)?;
    let tags: Vec<Value> = t
        .tags
        .iter()
        .map(|(k, v)| json!({ "Key": k, "Value": v }))
        .collect();
    Ok(json!({ "Tags": tags }))
}

impl Table {
    pub fn attr_type(&self, name: &str) -> Option<&str> {
        self.attr_defs
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, t)| t.as_str())
    }

    pub fn index(&self, name: &str) -> Result<&Index, AwsError> {
        self.gsis
            .iter()
            .chain(&self.lsis)
            .find(|i| i.name == name && i.status != "DELETING")
            .ok_or_else(|| {
                validation(format!(
                    "The table does not have the specified index: {name}"
                ))
            })
    }

    pub fn in_index(&self, index: &Index, item: &Item) -> bool {
        index.key.names().all(|k| item.contains_key(k))
    }

    fn key_value(&self, name: &str, a: &Attr, in_item: bool) -> Result<KeyVal, AwsError> {
        let want = self.attr_type(name).unwrap_or("S");
        if a.type_name() != want {
            return Err(if in_item {
                invalid(format!(
                    "Type mismatch for key {name} expected: {want} actual: {}",
                    a.type_name()
                ))
            } else {
                validation("The provided key element does not match the schema")
            });
        }
        match a {
            Attr::S(s) if s.is_empty() => Err(validation(format!(
                "One or more parameter values are not valid. The AttributeValue for a key attribute cannot contain an empty string value. Key: {name}"
            ))),
            Attr::B(b) if b.is_empty() => Err(validation(format!(
                "One or more parameter values are not valid. The AttributeValue for a key attribute cannot contain an empty binary value. Key: {name}"
            ))),
            Attr::S(_) | Attr::B(_) if name == self.key.hash && a.byte_size() > 2048 => Err(
                invalid("Size of hashkey has exceeded the maximum size limit of2048 bytes"),
            ),
            Attr::S(_) | Attr::B(_) if a.byte_size() > 1024 && name != self.key.hash => {
                Err(invalid(
                    "Aggregated size of all range keys has exceeded the size limit of 1024 bytes",
                ))
            }
            _ => Ok(KeyVal::of(a).expect("key attributes are scalars")),
        }
    }

    pub fn pk_of_item(&self, item: &Item) -> Result<Pk, AwsError> {
        let part = |name: &str| {
            let a = item
                .get(name)
                .ok_or_else(|| invalid(format!("Missing the key {name} in the item")))?;
            self.key_value(name, a, true)
        };
        let hash = part(&self.key.hash)?;
        let range = self.key.range.as_deref().map(part).transpose()?;
        for idx in self.gsis.iter().chain(&self.lsis) {
            for k in idx.key.names() {
                let Some(a) = item.get(k) else {
                    continue;
                };
                let want = self.attr_type(k).unwrap_or("S");
                if a.type_name() != want {
                    return Err(invalid(format!(
                        "Type mismatch for Index Key {k} Expected: {want} Actual: {} IndexName: {}",
                        a.type_name(),
                        idx.name
                    )));
                }
                if matches!(a, Attr::S(s) if s.is_empty())
                    || matches!(a, Attr::B(b) if b.is_empty())
                {
                    return Err(validation(format!(
                        "One or more parameter values are not valid. A value specified for a secondary index key is not supported. The AttributeValue for a key attribute cannot contain an empty string value. IndexName: {}, IndexKey: {k}",
                        idx.name
                    )));
                }
            }
        }
        if item_size(item) > 400 * 1024 {
            return Err(validation(
                "Item size has exceeded the maximum allowed size",
            ));
        }
        Ok((hash, range))
    }

    pub fn pk_of_key(&self, key: &Item) -> Result<Pk, AwsError> {
        let expected = 1 + usize::from(self.key.range.is_some());
        if key.len() != expected {
            return Err(validation(
                "The provided key element does not match the schema",
            ));
        }
        let part = |name: &str| {
            let a = key
                .get(name)
                .ok_or_else(|| validation("The provided key element does not match the schema"))?;
            self.key_value(name, a, false)
        };
        let hash = part(&self.key.hash)?;
        let range = self.key.range.as_deref().map(part).transpose()?;
        Ok((hash, range))
    }

    pub fn key_attrs(&self, item: &Item, index: Option<&Index>) -> Item {
        self.key
            .names()
            .chain(index.into_iter().flat_map(|i| i.key.names()))
            .filter_map(|k| Some((k.clone(), item.get(k)?.clone())))
            .collect()
    }

    pub fn index_view(&self, index: &Index, item: &Item) -> Item {
        if index.projection_type == "ALL" {
            return item.clone();
        }
        let mut out = self.key_attrs(item, Some(index));
        for k in &index.non_key {
            if let Some(v) = item.get(k) {
                out.insert(k.clone(), v.clone());
            }
        }
        out
    }
}

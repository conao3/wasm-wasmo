use serde_json::{Value, json};

use crate::codec::AwsError;
use crate::service::{Ctx, InputExt, OpResult};

use super::attr::item_size;
use super::table::{description, projection_value};
use super::{Backup, Region, Table, err, require_table_name, table_arn, table_name, validation};

fn secs(ctx: &Ctx<'_>) -> f64 {
    (ctx.now_ms() / 1000.0).floor()
}

fn required<'a>(input: &'a Value, key: &str) -> Result<&'a str, AwsError> {
    input.str_field(key).ok_or_else(|| {
        let mut field = key.to_string();
        field[..1].make_ascii_lowercase();
        validation(format!(
            "1 validation error detected: Value null at '{field}' failed to satisfy constraint: Member must not be null"
        ))
    })
}

fn backup_details(b: &Backup) -> Value {
    json!({
        "BackupArn": b.arn,
        "BackupName": b.name,
        "BackupSizeBytes": b.table.items.values().map(item_size).sum::<usize>(),
        "BackupStatus": "AVAILABLE",
        "BackupType": "USER",
        "BackupCreationDateTime": b.created,
    })
}

fn backup_description(b: &Backup) -> Value {
    let t = &b.table;
    let mut features = json!({});
    if !t.gsis.is_empty() {
        features["GlobalSecondaryIndexes"] = t
            .gsis
            .iter()
            .map(|i| json!({ "IndexName": i.name, "KeySchema": i.key.to_value(), "Projection": projection_value(i) }))
            .collect();
    }
    if !t.lsis.is_empty() {
        features["LocalSecondaryIndexes"] = t
            .lsis
            .iter()
            .map(|i| json!({ "IndexName": i.name, "KeySchema": i.key.to_value(), "Projection": projection_value(i) }))
            .collect();
    }
    json!({
        "BackupDetails": backup_details(b),
        "SourceTableDetails": {
            "TableName": t.name,
            "TableId": t.id,
            "TableArn": b.table_arn,
            "TableSizeBytes": t.items.values().map(item_size).sum::<usize>(),
            "KeySchema": t.key.to_value(),
            "TableCreationDateTime": t.created,
            "ProvisionedThroughput": { "ReadCapacityUnits": t.throughput.0, "WriteCapacityUnits": t.throughput.1 },
            "ItemCount": t.items.len(),
            "BillingMode": t.billing,
        },
        "SourceTableFeatureDetails": features,
    })
}

fn backup<'a>(region: &'a Region, input: &Value) -> Result<&'a Backup, AwsError> {
    let arn = required(input, "BackupArn")?;
    region.backups.get(arn).ok_or_else(|| {
        err(
            "BackupNotFoundException",
            format!("Backup not found: {arn}"),
        )
    })
}

pub fn create_backup(region: &mut Region, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let name = required(input, "BackupName")?.to_string();
    let t = region.table(require_table_name(input)?)?.clone();
    let suffix: String = ctx.uuid().chars().take(8).collect();
    let arn = format!(
        "{}/backup/{:017}-{suffix}",
        table_arn(ctx, &t.name),
        ctx.now_ms() as u64
    );
    let b = Backup {
        arn: arn.clone(),
        name,
        created: secs(ctx),
        table_arn: table_arn(ctx, &t.name),
        table: t,
    };
    let out = json!({ "BackupDetails": backup_details(&b) });
    region.backups.insert(arn, b);
    Ok(out)
}

pub fn describe_backup(region: &mut Region, _ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    Ok(json!({ "BackupDescription": backup_description(backup(region, input)?) }))
}

pub fn list_backups(region: &mut Region, _ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let filter = input.str_field("TableName").map(table_name);
    let lower = input.get("TimeRangeLowerBound").and_then(Value::as_f64);
    let upper = input.get("TimeRangeUpperBound").and_then(Value::as_f64);
    let start = input.str_field("ExclusiveStartBackupArn");
    let all: Vec<&Backup> = region
        .backups
        .values()
        .filter(|b| filter.is_none_or(|f| b.table.name == f))
        .filter(|b| lower.is_none_or(|l| b.created >= l) && upper.is_none_or(|u| b.created < u))
        .filter(|b| start.is_none_or(|s| b.arn.as_str() > s))
        .collect();
    let limit = input
        .i64_field("Limit")
        .map_or(all.len(), |l| l.max(1) as usize);
    let page = &all[..limit.min(all.len())];
    let summaries: Vec<Value> = page
        .iter()
        .map(|b| {
            let mut s = backup_details(b);
            s["TableName"] = json!(b.table.name);
            s["TableId"] = json!(b.table.id);
            s["TableArn"] = json!(b.table_arn);
            s
        })
        .collect();
    let mut out = json!({ "BackupSummaries": summaries });
    if page.len() < all.len()
        && let Some(last) = page.last()
    {
        out["LastEvaluatedBackupArn"] = json!(last.arn);
    }
    Ok(out)
}

pub fn delete_backup(region: &mut Region, _ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let arn = backup(region, input)?.arn.clone();
    let b = region.backups.remove(&arn).expect("backup exists");
    let mut d = backup_description(&b);
    d["BackupDetails"]["BackupStatus"] = json!("DELETED");
    Ok(json!({ "BackupDescription": d }))
}

fn restored(
    ctx: &mut Ctx<'_>,
    region: &Region,
    source: &Table,
    target: &str,
    summary: Value,
) -> Result<Table, AwsError> {
    if region.tables.contains_key(target) {
        return Err(err(
            "TableAlreadyExistsException",
            format!("Table already exists: {target}"),
        ));
    }
    let mut t = source.clone();
    t.name = target.to_string();
    t.id = ctx.uuid();
    t.created = secs(ctx);
    t.status = "CREATING".into();
    t.tags.clear();
    t.ttl = None;
    t.stream = None;
    t.pitr = false;
    t.deletion_protection = false;
    t.restore_summary = Some(summary);
    Ok(t)
}

pub fn restore_from_backup(region: &mut Region, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let target = required(input, "TargetTableName")?.to_string();
    let b = backup(region, input)?;
    let summary = json!({
        "SourceBackupArn": b.arn,
        "SourceTableArn": b.table_arn,
        "RestoreDateTime": secs(ctx),
        "RestoreInProgress": false,
    });
    let source = b.table.clone();
    let t = restored(ctx, region, &source, &target, summary)?;
    let out = json!({ "TableDescription": description(ctx, &t) });
    region.tables.insert(target, t);
    Ok(out)
}

pub fn restore_to_point_in_time(region: &mut Region, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let target = required(input, "TargetTableName")?.to_string();
    let source_name = input
        .str_field("SourceTableName")
        .or_else(|| input.str_field("SourceTableArn"))
        .ok_or_else(|| validation("Either SourceTableName or SourceTableArn must be specified"))?;
    let source = region
        .tables
        .get(table_name(source_name))
        .ok_or_else(|| {
            err(
                "TableNotFoundException",
                format!("Table not found: {}", table_name(source_name)),
            )
        })?
        .clone();
    if !source.pitr {
        return Err(err(
            "PointInTimeRecoveryUnavailableException",
            format!(
                "Point in time recovery is not enabled for table '{}'",
                source.name
            ),
        ));
    }
    let summary = json!({
        "SourceTableArn": table_arn(ctx, &source.name),
        "RestoreDateTime": input.get("RestoreDateTime").cloned().unwrap_or(json!(secs(ctx))),
        "RestoreInProgress": false,
    });
    let t = restored(ctx, region, &source, &target, summary)?;
    let out = json!({ "TableDescription": description(ctx, &t) });
    region.tables.insert(target, t);
    Ok(out)
}

fn continuous_backups(ctx: &Ctx<'_>, t: &Table) -> Value {
    let mut pitr = json!({
        "PointInTimeRecoveryStatus": if t.pitr { "ENABLED" } else { "DISABLED" },
    });
    if t.pitr {
        pitr["EarliestRestorableDateTime"] = json!(t.created);
        pitr["LatestRestorableDateTime"] = json!(secs(ctx));
        pitr["RecoveryPeriodInDays"] = json!(35);
    }
    json!({
        "ContinuousBackupsStatus": "ENABLED",
        "PointInTimeRecoveryDescription": pitr,
    })
}

pub fn update_continuous_backups(
    region: &mut Region,
    ctx: &mut Ctx<'_>,
    input: &Value,
) -> OpResult {
    let enabled = input
        .get("PointInTimeRecoverySpecification")
        .and_then(|s| s.get("PointInTimeRecoveryEnabled"))
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            validation(
                "1 validation error detected: Value null at 'pointInTimeRecoverySpecification' failed to satisfy constraint: Member must not be null",
            )
        })?;
    let t = region.table_mut(require_table_name(input)?)?;
    t.pitr = enabled;
    Ok(json!({ "ContinuousBackupsDescription": continuous_backups(ctx, t) }))
}

pub fn describe_continuous_backups(
    region: &mut Region,
    ctx: &mut Ctx<'_>,
    input: &Value,
) -> OpResult {
    let t = region
        .tables
        .get(table_name(require_table_name(input)?))
        .ok_or_else(|| {
            err(
                "TableNotFoundException",
                format!(
                    "Table not found: {}",
                    table_name(input.str_field("TableName").unwrap_or_default())
                ),
            )
        })?;
    Ok(json!({ "ContinuousBackupsDescription": continuous_backups(ctx, t) }))
}

fn policy_table<'a>(region: &Region, input: &'a Value) -> Result<&'a str, AwsError> {
    let arn = required(input, "ResourceArn")?;
    region.table(arn)?;
    Ok(arn)
}

fn no_policy(arn: &str) -> AwsError {
    err(
        "PolicyNotFoundException",
        format!("Resource-based policy not found for the provided ResourceArn: {arn}"),
    )
}

pub fn put_policy(region: &mut Region, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let arn = policy_table(region, input)?.to_string();
    let policy = required(input, "Policy")?.to_string();
    if serde_json::from_str::<Value>(&policy).is_err() {
        return Err(validation(
            "Invalid policy document: the policy is not valid JSON",
        ));
    }
    let existing = region.policies.get(&arn);
    match input.str_field("ExpectedRevisionId") {
        Some("NO_POLICY") if existing.is_some() => {
            return Err(err(
                "PolicyNotFoundException",
                format!(
                    "Resource-based policy not found for the provided ResourceArn: Requested policy update expecting none to be present for table {}, but a policy exists with revision id {}.",
                    table_name(&arn),
                    existing.map(|p| p.1.as_str()).unwrap_or_default()
                ),
            ));
        }
        Some(rev) if rev != "NO_POLICY" && existing.is_none_or(|p| p.1 != rev) => {
            return Err(err(
                "PolicyNotFoundException",
                format!(
                    "Resource-based policy not found for the provided ResourceArn: Requested update for policy with revision id {rev}, but the policy associated to target table {} has revision id {}.",
                    table_name(&arn),
                    existing.map_or("none", |p| p.1.as_str())
                ),
            ));
        }
        _ => {}
    }
    if let Some((p, rev)) = existing
        && *p == policy
    {
        return Ok(json!({ "RevisionId": rev }));
    }
    let rev = format!(
        "{}",
        ctx.now_ms() as u64 * 1000 + region.policies.len() as u64
    );
    region.policies.insert(arn, (policy, rev.clone()));
    Ok(json!({ "RevisionId": rev }))
}

pub fn get_policy(region: &mut Region, input: &Value) -> OpResult {
    let arn = policy_table(region, input)?;
    let (policy, rev) = region.policies.get(arn).ok_or_else(|| no_policy(arn))?;
    Ok(json!({ "Policy": policy, "RevisionId": rev }))
}

pub fn delete_policy(region: &mut Region, input: &Value) -> OpResult {
    let arn = policy_table(region, input)?.to_string();
    let existing = region.policies.get(&arn).ok_or_else(|| no_policy(&arn))?;
    if let Some(rev) = input.str_field("ExpectedRevisionId")
        && existing.1 != rev
    {
        return Err(err(
            "PolicyNotFoundException",
            format!(
                "Resource-based policy not found for the provided ResourceArn: Requested update for policy with revision id {rev}, but the policy associated to target table {} has revision id {}.",
                table_name(&arn),
                existing.1
            ),
        ));
    }
    let (_, rev) = region.policies.remove(&arn).expect("policy exists");
    Ok(json!({ "RevisionId": rev }))
}

fn global_table_description(ctx: &Ctx<'_>, name: &str, replicas: &[String], created: f64) -> Value {
    json!({
        "GlobalTableName": name,
        "GlobalTableArn": format!("arn:aws:dynamodb::{}:global-table/{name}", ctx.account),
        "GlobalTableStatus": "ACTIVE",
        "CreationDateTime": created,
        "ReplicationGroup": replicas.iter().map(|r| json!({ "RegionName": r, "ReplicaStatus": "ACTIVE" })).collect::<Vec<_>>(),
    })
}

fn global_table_not_found(name: &str) -> AwsError {
    err(
        "GlobalTableNotFoundException",
        format!("Global table not found: {name}"),
    )
}

fn regions_of(list: &[Value]) -> Vec<String> {
    list.iter()
        .filter_map(|r| r.str_field("RegionName"))
        .map(str::to_string)
        .collect()
}

pub fn create_global_table(region: &mut Region, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let name = required(input, "GlobalTableName")?.to_string();
    if region.global_tables.contains_key(&name) {
        return Err(err(
            "GlobalTableAlreadyExistsException",
            format!("Global table already exists: {name}"),
        ));
    }
    region
        .table(&name)
        .map_err(|_| err("TableNotFoundException", format!("Table not found: {name}")))?;
    let replicas = regions_of(input.array_field("ReplicationGroup"));
    let created = secs(ctx);
    let out = json!({ "GlobalTableDescription": global_table_description(ctx, &name, &replicas, created) });
    region.global_tables.insert(name, (replicas, created));
    Ok(out)
}

pub fn describe_global_table(region: &mut Region, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let name = required(input, "GlobalTableName")?;
    let (replicas, created) = region
        .global_tables
        .get(name)
        .ok_or_else(|| global_table_not_found(name))?;
    Ok(json!({ "GlobalTableDescription": global_table_description(ctx, name, replicas, *created) }))
}

pub fn update_global_table(region: &mut Region, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let name = required(input, "GlobalTableName")?.to_string();
    let (replicas, created) = region
        .global_tables
        .get_mut(&name)
        .ok_or_else(|| global_table_not_found(&name))?;
    for u in input.array_field("ReplicaUpdates") {
        if let Some(r) = u.get("Create").and_then(|c| c.str_field("RegionName")) {
            if replicas.iter().any(|x| x == r) {
                return Err(err(
                    "ReplicaAlreadyExistsException",
                    format!("Replica already exists: {r}"),
                ));
            }
            replicas.push(r.to_string());
        } else if let Some(r) = u.get("Delete").and_then(|c| c.str_field("RegionName")) {
            let before = replicas.len();
            replicas.retain(|x| x != r);
            if replicas.len() == before {
                return Err(err(
                    "ReplicaNotFoundException",
                    format!("Replica not found: {r}"),
                ));
            }
        }
    }
    Ok(
        json!({ "GlobalTableDescription": global_table_description(ctx, &name, replicas, *created) }),
    )
}

pub fn list_global_tables(region: &mut Region, input: &Value) -> OpResult {
    let filter = input.str_field("RegionName");
    let tables: Vec<Value> = region
        .global_tables
        .iter()
        .filter(|(_, (r, _))| filter.is_none_or(|f| r.iter().any(|x| x == f)))
        .map(|(n, (r, _))| json!({ "GlobalTableName": n, "ReplicationGroup": r.iter().map(|x| json!({ "RegionName": x })).collect::<Vec<_>>() }))
        .collect();
    Ok(json!({ "GlobalTables": tables }))
}

pub fn global_table_settings(region: &mut Region, input: &Value) -> OpResult {
    let name = required(input, "GlobalTableName")?;
    let (replicas, _) = region
        .global_tables
        .get(name)
        .ok_or_else(|| global_table_not_found(name))?;
    Ok(json!({
        "GlobalTableName": name,
        "ReplicaSettings": replicas.iter().map(|r| json!({
            "RegionName": r,
            "ReplicaStatus": "ACTIVE",
            "ReplicaBillingModeSummary": { "BillingMode": "PAY_PER_REQUEST" },
            "ReplicaProvisionedReadCapacityUnits": 0,
            "ReplicaProvisionedWriteCapacityUnits": 0,
        })).collect::<Vec<_>>(),
    }))
}

pub fn export_table(region: &mut Region, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let arn = required(input, "TableArn")?.to_string();
    let bucket = required(input, "S3Bucket")?.to_string();
    let t = region.table(&arn)?;
    if !t.pitr {
        return Err(err(
            "PointInTimeRecoveryUnavailableException",
            format!(
                "Point in time recovery is not enabled for table '{}'",
                t.name
            ),
        ));
    }
    let export_arn = format!(
        "{arn}/export/{:017}-{}",
        ctx.now_ms() as u64,
        &ctx.uuid()[..8]
    );
    let now = secs(ctx);
    let d = json!({
        "ExportArn": export_arn,
        "ExportStatus": "COMPLETED",
        "StartTime": now,
        "EndTime": now,
        "ExportTime": input.get("ExportTime").cloned().unwrap_or(json!(now)),
        "TableArn": arn,
        "TableId": t.id,
        "S3Bucket": bucket,
        "S3Prefix": input.str_field("S3Prefix").unwrap_or_default(),
        "S3SseAlgorithm": input.str_field("S3SseAlgorithm").unwrap_or("AES256"),
        "ExportFormat": input.str_field("ExportFormat").unwrap_or("DYNAMODB_JSON"),
        "ExportType": input.str_field("ExportType").unwrap_or("FULL_EXPORT"),
        "ExportManifest": format!("AWSDynamoDB/{}/manifest-summary.json", &export_arn[export_arn.len() - 26..]),
        "BilledSizeBytes": t.items.values().map(item_size).sum::<usize>(),
        "ItemCount": t.items.len(),
    });
    region.exports.insert(export_arn, d.clone());
    Ok(json!({ "ExportDescription": d }))
}

pub fn describe_export(region: &mut Region, input: &Value) -> OpResult {
    let arn = required(input, "ExportArn")?;
    let d = region.exports.get(arn).ok_or_else(|| {
        err(
            "ExportNotFoundException",
            format!("Export not found: {arn}"),
        )
    })?;
    Ok(json!({ "ExportDescription": d }))
}

pub fn list_exports(region: &mut Region, input: &Value) -> OpResult {
    let filter = input.str_field("TableArn");
    let summaries: Vec<Value> = region
        .exports
        .values()
        .filter(|d| filter.is_none_or(|f| d["TableArn"] == f))
        .map(|d| json!({ "ExportArn": d["ExportArn"], "ExportStatus": d["ExportStatus"], "ExportType": d["ExportType"] }))
        .collect();
    Ok(json!({ "ExportSummaries": summaries }))
}

pub fn import_table(region: &mut Region, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let params = input
        .get("TableCreationParameters")
        .ok_or_else(|| validation("TableCreationParameters is required"))?;
    let name = require_table_name(params)?.to_string();
    if region.tables.contains_key(&name) {
        return Err(err(
            "ResourceInUseException",
            format!("Table already exists: {name}"),
        ));
    }
    let mut create = params.clone();
    if create.get("BillingMode").is_none() && create.get("ProvisionedThroughput").is_none() {
        create["BillingMode"] = json!("PAY_PER_REQUEST");
    }
    super::table::create(region, ctx, &create)?;
    let arn = table_arn(ctx, &name);
    let now = secs(ctx);
    let import_arn = format!(
        "{arn}/import/{:017}-{}",
        ctx.now_ms() as u64,
        &ctx.uuid()[..8]
    );
    let d = json!({
        "ImportArn": import_arn,
        "ImportStatus": "COMPLETED",
        "TableArn": arn,
        "TableId": region.tables[&name].id,
        "S3BucketSource": input.get("S3BucketSource").cloned().unwrap_or(json!({})),
        "InputFormat": input.str_field("InputFormat").unwrap_or("DYNAMODB_JSON"),
        "InputCompressionType": input.str_field("InputCompressionType").unwrap_or("NONE"),
        "TableCreationParameters": params,
        "StartTime": now,
        "EndTime": now,
        "ProcessedSizeBytes": 0,
        "ProcessedItemCount": 0,
        "ImportedItemCount": 0,
        "ErrorCount": 0,
    });
    region.imports.insert(import_arn, d.clone());
    Ok(json!({ "ImportTableDescription": d }))
}

pub fn describe_import(region: &mut Region, input: &Value) -> OpResult {
    let arn = required(input, "ImportArn")?;
    let d = region.imports.get(arn).ok_or_else(|| {
        err(
            "ImportNotFoundException",
            format!("Import not found: {arn}"),
        )
    })?;
    Ok(json!({ "ImportTableDescription": d }))
}

pub fn list_imports(region: &mut Region, input: &Value) -> OpResult {
    let filter = input.str_field("TableArn");
    let list: Vec<Value> = region
        .imports
        .values()
        .filter(|d| filter.is_none_or(|f| d["TableArn"] == f))
        .map(|d| {
            json!({
                "ImportArn": d["ImportArn"],
                "ImportStatus": d["ImportStatus"],
                "TableArn": d["TableArn"],
                "S3BucketSource": d["S3BucketSource"],
                "InputFormat": d["InputFormat"],
                "StartTime": d["StartTime"],
                "EndTime": d["EndTime"],
            })
        })
        .collect();
    Ok(json!({ "ImportSummaryList": list }))
}

pub fn kinesis_destination(region: &mut Region, operation: &str, input: &Value) -> OpResult {
    let name = table_name(require_table_name(input)?).to_string();
    let stream = required(input, "StreamArn")?.to_string();
    region.table(&name)?;
    let dests = region.kinesis.entry(name.clone()).or_default();
    let pos = dests.iter().position(|d| d["StreamArn"] == stream.as_str());
    let status = match (operation, pos) {
        ("EnableKinesisStreamingDestination", Some(p))
            if dests[p]["DestinationStatus"] == "ACTIVE" =>
        {
            return Err(validation(format!(
                "Table {name} already has an active Kinesis streaming destination for stream {stream}"
            )));
        }
        ("EnableKinesisStreamingDestination", _) => {
            let precision = input
                .get("EnableKinesisStreamingConfiguration")
                .and_then(|c| c.str_field("ApproximateCreationDateTimePrecision"))
                .unwrap_or("MILLISECOND");
            let d = json!({
                "StreamArn": stream,
                "DestinationStatus": "ACTIVE",
                "ApproximateCreationDateTimePrecision": precision,
            });
            match pos {
                Some(p) => dests[p] = d,
                None => dests.push(d),
            }
            "ENABLING"
        }
        (_, None) => {
            return Err(err(
                "ResourceNotFoundException",
                format!(
                    "Requested resource not found: Kinesis streaming destination {stream} not found"
                ),
            ));
        }
        ("DisableKinesisStreamingDestination", Some(p)) => {
            dests[p]["DestinationStatus"] = json!("DISABLED");
            "DISABLING"
        }
        (_, Some(p)) => {
            if let Some(prec) = input
                .get("UpdateKinesisStreamingConfiguration")
                .and_then(|c| c.str_field("ApproximateCreationDateTimePrecision"))
            {
                dests[p]["ApproximateCreationDateTimePrecision"] = json!(prec);
            }
            "UPDATING"
        }
    };
    let mut out = json!({ "TableName": name, "StreamArn": stream, "DestinationStatus": status });
    for key in [
        "EnableKinesisStreamingConfiguration",
        "UpdateKinesisStreamingConfiguration",
    ] {
        if let Some(c) = input.get(key) {
            out[key] = c.clone();
        }
    }
    Ok(out)
}

pub fn describe_kinesis(region: &mut Region, input: &Value) -> OpResult {
    let name = table_name(require_table_name(input)?).to_string();
    region.table(&name)?;
    let dests = region.kinesis.get(&name).cloned().unwrap_or_default();
    Ok(json!({ "TableName": name, "KinesisDataStreamDestinations": dests }))
}

fn insight_key(region: &Region, input: &Value) -> Result<(String, Option<String>), AwsError> {
    let t = region.table(require_table_name(input)?)?;
    let index = input.str_field("IndexName").map(str::to_string);
    if let Some(i) = &index {
        t.index(i)?;
    }
    Ok((t.name.clone(), index))
}

fn insight_value(key: &(String, Option<String>), status: &str) -> Value {
    let mut v = json!({
        "TableName": key.0,
        "ContributorInsightsStatus": status,
        "ContributorInsightsMode": "ACCESSED_AND_THROTTLED_KEYS",
    });
    if let Some(i) = &key.1 {
        v["IndexName"] = json!(i);
    }
    v
}

pub fn describe_insights(region: &mut Region, input: &Value) -> OpResult {
    let key = insight_key(region, input)?;
    let status = region.insights.get(&key).map_or("DISABLED", String::as_str);
    let mut v = insight_value(&key, status);
    if status == "ENABLED" {
        let rule = format!("DynamoDBContributorInsights-PKC-{}", key.0);
        v["ContributorInsightsRuleList"] = json!([rule]);
    }
    Ok(v)
}

pub fn update_insights(region: &mut Region, input: &Value) -> OpResult {
    let key = insight_key(region, input)?;
    let status = match required(input, "ContributorInsightsAction")? {
        "ENABLE" => "ENABLED",
        "DISABLE" => "DISABLED",
        other => {
            return Err(validation(format!(
                "1 validation error detected: Value '{other}' at 'contributorInsightsAction' failed to satisfy constraint: Member must satisfy enum value set: [ENABLE, DISABLE]"
            )));
        }
    };
    region.insights.insert(key.clone(), status.to_string());
    Ok(insight_value(&key, status))
}

pub fn list_insights(region: &mut Region, input: &Value) -> OpResult {
    let filter = input.str_field("TableName").map(table_name);
    let list: Vec<Value> = region
        .insights
        .iter()
        .filter(|((t, _), _)| filter.is_none_or(|f| t == f))
        .map(|(k, s)| insight_value(k, s))
        .collect();
    Ok(json!({ "ContributorInsightsSummaries": list }))
}

pub fn replica_auto_scaling(region: &mut Region, input: &Value) -> OpResult {
    let t = region.table(require_table_name(input)?)?;
    Ok(json!({
        "TableAutoScalingDescription": {
            "TableName": t.name,
            "TableStatus": t.status,
            "Replicas": [],
        }
    }))
}

mod admin;
mod attr;
mod expr;
mod items;
mod legacy;
mod number;
mod partiql;
mod table;

use std::collections::{BTreeMap, HashMap};

use serde_json::{Value, json};

use crate::codec::AwsError;
use crate::embedded_model;
use crate::service::{Ctx, OpResult, Service, ServiceModel};

use attr::{Attr, Item};
use number::Num;

const TTL_HORIZON_SECS: f64 = 5.0 * 365.0 * 86400.0;

pub(crate) fn validation(message: impl Into<String>) -> AwsError {
    AwsError::new(400, "ValidationException", message)
}

fn err(code: &str, message: impl Into<String>) -> AwsError {
    AwsError::new(400, code, message)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeyVal {
    S(String),
    N(Num),
    B(Vec<u8>),
}

impl KeyVal {
    fn of(a: &Attr) -> Option<Self> {
        Some(match a {
            Attr::S(s) => KeyVal::S(s.clone()),
            Attr::N(_) => KeyVal::N(a.num()?),
            Attr::B(b) => KeyVal::B(b.clone()),
            _ => return None,
        })
    }

    fn bytes(&self) -> Vec<u8> {
        match self {
            KeyVal::S(s) => s.as_bytes().to_vec(),
            KeyVal::N(n) => n.to_string().into_bytes(),
            KeyVal::B(b) => b.clone(),
        }
    }
}

pub type Pk = (KeyVal, Option<KeyVal>);

#[derive(Debug, Clone)]
pub struct KeySchema {
    pub hash: String,
    pub range: Option<String>,
}

impl KeySchema {
    fn names(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.hash).chain(self.range.as_ref())
    }

    fn to_value(&self) -> Value {
        let mut v = vec![json!({ "AttributeName": self.hash, "KeyType": "HASH" })];
        if let Some(r) = &self.range {
            v.push(json!({ "AttributeName": r, "KeyType": "RANGE" }));
        }
        Value::Array(v)
    }
}

#[derive(Debug, Clone)]
pub struct Index {
    pub name: String,
    pub key: KeySchema,
    pub projection_type: String,
    pub non_key: Vec<String>,
    pub throughput: Option<(i64, i64)>,
    pub status: String,
    pub global: bool,
}

#[derive(Debug, Clone)]
pub struct Table {
    pub name: String,
    pub id: String,
    pub created: f64,
    pub status: String,
    pub attr_defs: Vec<(String, String)>,
    pub key: KeySchema,
    pub gsis: Vec<Index>,
    pub lsis: Vec<Index>,
    pub billing: String,
    pub billing_changed: f64,
    pub throughput: (i64, i64),
    pub items: BTreeMap<Pk, Item>,
    pub tags: Vec<(String, String)>,
    pub ttl: Option<String>,
    pub stream: Option<(bool, String, String)>,
    pub pitr: bool,
    pub deletion_protection: bool,
    pub table_class: Option<String>,
    pub sse: Option<Value>,
    pub restore_summary: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct Backup {
    pub arn: String,
    pub name: String,
    pub created: f64,
    pub table_arn: String,
    pub table: Table,
}

#[derive(Debug, Default)]
pub struct Region {
    pub tables: BTreeMap<String, Table>,
    pub backups: BTreeMap<String, Backup>,
    pub tx_tokens: HashMap<String, (Value, f64)>,
    pub policies: BTreeMap<String, (String, String)>,
    pub global_tables: BTreeMap<String, (Vec<String>, f64)>,
    pub exports: BTreeMap<String, Value>,
    pub imports: BTreeMap<String, Value>,
    pub insights: BTreeMap<(String, Option<String>), String>,
    pub kinesis: BTreeMap<String, Vec<Value>>,
}

impl Region {
    fn tick(&mut self, now_ms: f64) {
        let now = now_ms / 1000.0;
        for t in self.tables.values_mut() {
            if t.status != "ACTIVE" {
                t.status = "ACTIVE".into();
            }
            let before = t.gsis.len();
            t.gsis.retain(|i| i.status != "DELETING");
            if t.gsis.len() != before {
                let used: Vec<String> = t
                    .key
                    .names()
                    .chain(t.gsis.iter().chain(&t.lsis).flat_map(|i| i.key.names()))
                    .cloned()
                    .collect();
                t.attr_defs.retain(|(n, _)| used.contains(n));
            }
            for i in t.gsis.iter_mut().filter(|i| i.status != "ACTIVE") {
                i.status = "ACTIVE".into();
            }
            if let Some(attr) = &t.ttl {
                t.items.retain(|_, item| !expired(item, attr, now));
            }
        }
    }

    pub fn table(&self, name: &str) -> Result<&Table, AwsError> {
        self.tables.get(table_name(name)).ok_or_else(not_found)
    }

    pub fn table_mut(&mut self, name: &str) -> Result<&mut Table, AwsError> {
        self.tables.get_mut(table_name(name)).ok_or_else(not_found)
    }
}

fn expired(item: &Item, attr: &str, now: f64) -> bool {
    item.get(attr)
        .and_then(Attr::num)
        .map(|n| n.to_f64())
        .is_some_and(|t| t <= now && t > now - TTL_HORIZON_SECS)
}

fn not_found() -> AwsError {
    err("ResourceNotFoundException", "Requested resource not found")
}

pub fn table_name(name: &str) -> &str {
    match name.split_once(":table/") {
        Some((_, rest)) => rest.split('/').next().unwrap_or(rest),
        None => name,
    }
}

pub fn table_arn(ctx: &Ctx<'_>, name: &str) -> String {
    format!(
        "arn:aws:dynamodb:{}:{}:table/{name}",
        ctx.region, ctx.account
    )
}

pub fn require_table_name(input: &Value) -> Result<&str, AwsError> {
    input
        .get("TableName")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            validation(
                "1 validation error detected: Value null at 'tableName' failed to satisfy constraint: Member must not be null",
            )
        })
}

#[derive(Default)]
pub struct DynamoDb {
    regions: HashMap<(String, String), Region>,
}

impl Service for DynamoDb {
    fn signing_name(&self) -> &'static str {
        "dynamodb"
    }

    fn model(&self) -> &'static ServiceModel {
        embedded_model!("dynamodb")
    }

    fn call(&mut self, ctx: &mut Ctx<'_>, operation: &str, input: Value) -> OpResult {
        let region = self
            .regions
            .entry((ctx.account.clone(), ctx.region.clone()))
            .or_default();
        region.tick(ctx.now_ms());
        let input = &input;
        match operation {
            "CreateTable" => table::create(region, ctx, input),
            "DescribeTable" => table::describe(region, ctx, input),
            "ListTables" => table::list(region, input),
            "UpdateTable" => table::update(region, ctx, input),
            "DeleteTable" => table::delete(region, ctx, input),
            "UpdateTimeToLive" => table::update_ttl(region, input),
            "DescribeTimeToLive" => table::describe_ttl(region, input),
            "TagResource" => table::tag(region, input),
            "UntagResource" => table::untag(region, input),
            "ListTagsOfResource" => table::list_tags(region, input),
            "DescribeLimits" => Ok(json!({
                "AccountMaxReadCapacityUnits": 80000,
                "AccountMaxWriteCapacityUnits": 80000,
                "TableMaxReadCapacityUnits": 40000,
                "TableMaxWriteCapacityUnits": 40000,
            })),
            "DescribeEndpoints" => Ok(json!({
                "Endpoints": [{
                    "Address": format!("dynamodb.{}.amazonaws.com", ctx.region),
                    "CachePeriodInMinutes": 1440,
                }],
            })),
            "PutItem" => items::put(region, input),
            "GetItem" => items::get(region, input),
            "UpdateItem" => items::update(region, input),
            "DeleteItem" => items::delete(region, input),
            "Query" => items::query(region, input),
            "Scan" => items::scan(region, input),
            "BatchGetItem" => items::batch_get(region, input),
            "BatchWriteItem" => items::batch_write(region, input),
            "TransactGetItems" => items::transact_get(region, input),
            "TransactWriteItems" => items::transact_write(region, ctx, input),
            "ExecuteStatement" => partiql::execute(region, input),
            "BatchExecuteStatement" => partiql::batch_execute(region, input),
            "ExecuteTransaction" => partiql::execute_transaction(region, input),
            "CreateBackup" => admin::create_backup(region, ctx, input),
            "DescribeBackup" => admin::describe_backup(region, ctx, input),
            "ListBackups" => admin::list_backups(region, ctx, input),
            "DeleteBackup" => admin::delete_backup(region, ctx, input),
            "RestoreTableFromBackup" => admin::restore_from_backup(region, ctx, input),
            "RestoreTableToPointInTime" => admin::restore_to_point_in_time(region, ctx, input),
            "UpdateContinuousBackups" => admin::update_continuous_backups(region, ctx, input),
            "DescribeContinuousBackups" => admin::describe_continuous_backups(region, ctx, input),
            "PutResourcePolicy" => admin::put_policy(region, ctx, input),
            "GetResourcePolicy" => admin::get_policy(region, input),
            "DeleteResourcePolicy" => admin::delete_policy(region, input),
            "CreateGlobalTable" => admin::create_global_table(region, ctx, input),
            "DescribeGlobalTable" => admin::describe_global_table(region, ctx, input),
            "UpdateGlobalTable" => admin::update_global_table(region, ctx, input),
            "ListGlobalTables" => admin::list_global_tables(region, input),
            "DescribeGlobalTableSettings" | "UpdateGlobalTableSettings" => {
                admin::global_table_settings(region, input)
            }
            "ExportTableToPointInTime" => admin::export_table(region, ctx, input),
            "DescribeExport" => admin::describe_export(region, input),
            "ListExports" => admin::list_exports(region, input),
            "ImportTable" => admin::import_table(region, ctx, input),
            "DescribeImport" => admin::describe_import(region, input),
            "ListImports" => admin::list_imports(region, input),
            "EnableKinesisStreamingDestination"
            | "DisableKinesisStreamingDestination"
            | "UpdateKinesisStreamingDestination" => {
                admin::kinesis_destination(region, operation, input)
            }
            "DescribeKinesisStreamingDestination" => admin::describe_kinesis(region, input),
            "DescribeContributorInsights" => admin::describe_insights(region, input),
            "UpdateContributorInsights" => admin::update_insights(region, input),
            "ListContributorInsights" => admin::list_insights(region, input),
            "DescribeTableReplicaAutoScaling" | "UpdateTableReplicaAutoScaling" => {
                admin::replica_auto_scaling(region, input)
            }
            other => Err(err(
                "UnknownOperationException",
                format!("wasmo: {other} is not implemented"),
            )),
        }
    }
}

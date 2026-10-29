use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use crate::codec::AwsError;
use crate::service::{Ctx, InputExt, OpResult};

use super::attr::{Attr, Item, item_from_value, item_size, item_to_value};
use super::expr::eval::{apply, matches, project};
use super::expr::{
    Bindings, CmpOp, Cond, KeyCond, Path, RangeCond, Update, parse_condition, parse_key_condition,
    parse_projection, parse_update,
};
use super::{
    Index, KeyVal, Pk, Region, Table, err, legacy, require_table_name, table_name, validation,
};

const PAGE_BYTES: usize = 1024 * 1024;
const TOKEN_WINDOW_MS: f64 = 10.0 * 60.0 * 1000.0;

const LEGACY_PARAMS: &[&str] = &[
    "AttributesToGet",
    "AttributeUpdates",
    "ConditionalOperator",
    "Expected",
    "KeyConditions",
    "QueryFilter",
    "ScanFilter",
];
const EXPRESSION_PARAMS: &[&str] = &[
    "ConditionExpression",
    "FilterExpression",
    "KeyConditionExpression",
    "ProjectionExpression",
    "UpdateExpression",
];

fn present<'a>(input: &Value, keys: &[&'a str]) -> Vec<&'a str> {
    keys.iter()
        .copied()
        .filter(|k| input.get(*k).is_some_and(|v| !v.is_null()))
        .collect()
}

fn check_mixing(input: &Value) -> Result<(), AwsError> {
    let legacy = present(input, LEGACY_PARAMS);
    let modern = present(input, EXPRESSION_PARAMS);
    if !legacy.is_empty() && !modern.is_empty() {
        return Err(validation(format!(
            "Can not use both expression and non-expression parameters in the same request: Non-expression parameters: {{{}}} Expression parameters: {{{}}}",
            legacy.join(", "),
            modern.join(", ")
        )));
    }
    Ok(())
}

fn condition(input: &Value, b: &mut Bindings) -> Result<Option<Cond>, AwsError> {
    match input.str_field("ConditionExpression") {
        Some(e) => Ok(Some(parse_condition("ConditionExpression", e, b)?)),
        None => legacy::expected(input),
    }
}

fn filter(input: &Value, b: &mut Bindings, legacy_key: &str) -> Result<Option<Cond>, AwsError> {
    match input.str_field("FilterExpression") {
        Some(e) => Ok(Some(parse_condition("FilterExpression", e, b)?)),
        None => legacy::filter(input, legacy_key),
    }
}

fn projection(input: &Value, b: &mut Bindings) -> Result<Option<Vec<Path>>, AwsError> {
    match input.str_field("ProjectionExpression") {
        Some(e) => Ok(Some(parse_projection(e, b)?)),
        None => Ok(legacy::attributes_to_get(input)),
    }
}

fn condition_failed(input: &Value, old: Option<&Item>) -> AwsError {
    let mut e = err(
        "ConditionalCheckFailedException",
        "The conditional request failed",
    );
    if input.str_field("ReturnValuesOnConditionCheckFailure") == Some("ALL_OLD")
        && let Some(o) = old
    {
        e.data.insert("Item".into(), item_to_value(o));
    }
    e
}

fn passes(cond: &Option<Cond>, old: Option<&Item>) -> bool {
    let empty = Item::new();
    cond.as_ref()
        .is_none_or(|c| matches(c, old.unwrap_or(&empty)))
}

fn return_values<'a>(input: &'a Value, allowed: &[&str]) -> Result<&'a str, AwsError> {
    let rv = input.str_field("ReturnValues").unwrap_or("NONE");
    if !allowed.contains(&rv) {
        return Err(validation("Return values set to invalid value"));
    }
    Ok(rv)
}

fn units(bytes: usize, per: usize) -> f64 {
    bytes.div_ceil(per).max(1) as f64
}

fn with_capacity(mut out: Value, input: &Value, table: &str, capacity: f64) -> Value {
    match input.str_field("ReturnConsumedCapacity") {
        Some("TOTAL") => {
            out["ConsumedCapacity"] = json!({ "TableName": table, "CapacityUnits": capacity });
        }
        Some("INDEXES") => {
            out["ConsumedCapacity"] = json!({
                "TableName": table,
                "CapacityUnits": capacity,
                "Table": { "CapacityUnits": capacity },
            });
        }
        _ => {}
    }
    out
}

fn read_capacity(bytes: usize, input: &Value) -> f64 {
    let consistent = input.get("ConsistentRead").and_then(Value::as_bool) == Some(true);
    units(bytes, 4096) * if consistent { 1.0 } else { 0.5 }
}

fn required_item<'a>(input: &'a Value, key: &str) -> Result<&'a Value, AwsError> {
    input.get(key).ok_or_else(|| {
        validation(format!(
            "1 validation error detected: Value null at '{}' failed to satisfy constraint: Member must not be null",
            key.to_ascii_lowercase()
        ))
    })
}

pub fn put(region: &mut Region, input: &Value) -> OpResult {
    let name = table_name(require_table_name(input)?).to_string();
    let rv = return_values(input, &["NONE", "ALL_OLD"])?;
    check_mixing(input)?;
    let mut b = Bindings::from_input(input)?;
    let cond = condition(input, &mut b)?;
    b.check_unused()?;
    let t = region.table_mut(&name)?;
    let item = item_from_value(required_item(input, "Item")?)?;
    let pk = t.pk_of_item(&item)?;
    let old = t.items.get(&pk);
    if !passes(&cond, old) {
        return Err(condition_failed(input, old));
    }
    let size = item_size(&item).max(old.map_or(0, item_size));
    let old = t.items.insert(pk, item);
    let mut out = json!({});
    if rv == "ALL_OLD"
        && let Some(o) = old
    {
        out["Attributes"] = item_to_value(&o);
    }
    Ok(with_capacity(out, input, &name, units(size, 1024)))
}

pub fn get(region: &mut Region, input: &Value) -> OpResult {
    let name = table_name(require_table_name(input)?).to_string();
    check_mixing(input)?;
    let mut b = Bindings::from_input(input)?;
    let proj = projection(input, &mut b)?;
    b.check_unused()?;
    let t = region.table(&name)?;
    let key = item_from_value(required_item(input, "Key")?)?;
    let pk = t.pk_of_key(&key)?;
    let mut out = json!({});
    let mut size = 0;
    if let Some(item) = t.items.get(&pk) {
        size = item_size(item);
        out["Item"] = item_to_value(&match &proj {
            Some(p) => project(item, p),
            None => item.clone(),
        });
    }
    Ok(with_capacity(out, input, &name, read_capacity(size, input)))
}

fn check_key_untouched(t: &Table, paths: &[&Path]) -> Result<(), AwsError> {
    if let Some(p) = paths.iter().find(|p| t.key.names().any(|k| *k == p.root)) {
        return Err(validation(format!(
            "One or more parameter values were invalid: Cannot update attribute {}. This attribute is part of the key",
            p.root
        )));
    }
    Ok(())
}

enum Changes {
    Expression(Update),
    Legacy(Map<String, Value>),
    Nothing,
}

fn changes(input: &Value, b: &mut Bindings) -> Result<Changes, AwsError> {
    if let Some(e) = input.str_field("UpdateExpression") {
        return Ok(Changes::Expression(parse_update(e, b)?));
    }
    Ok(match input.obj_field("AttributeUpdates") {
        Some(m) => Changes::Legacy(m.clone()),
        None => Changes::Nothing,
    })
}

fn apply_changes(t: &Table, changes: &Changes, base: &Item) -> Result<(Item, Vec<Path>), AwsError> {
    match changes {
        Changes::Expression(u) => {
            check_key_untouched(t, &u.targets())?;
            let paths = u.targets().into_iter().cloned().collect();
            Ok((apply(u, base)?, paths))
        }
        Changes::Legacy(m) => {
            let roots: Vec<Path> = m.keys().map(|k| Path::top(k)).collect();
            check_key_untouched(t, &roots.iter().collect::<Vec<_>>())?;
            legacy::attribute_updates(m, base)
        }
        Changes::Nothing => Ok((base.clone(), Vec::new())),
    }
}

pub fn update(region: &mut Region, input: &Value) -> OpResult {
    let name = table_name(require_table_name(input)?).to_string();
    let rv = return_values(
        input,
        &["NONE", "ALL_OLD", "UPDATED_OLD", "ALL_NEW", "UPDATED_NEW"],
    )?;
    check_mixing(input)?;
    let mut b = Bindings::from_input(input)?;
    let changes = changes(input, &mut b)?;
    let cond = condition(input, &mut b)?;
    b.check_unused()?;
    let t = region.table_mut(&name)?;
    let key = item_from_value(required_item(input, "Key")?)?;
    let pk = t.pk_of_key(&key)?;
    let old = t.items.get(&pk);
    if !passes(&cond, old) {
        return Err(condition_failed(input, old));
    }
    let base = old.cloned().unwrap_or(key);
    let (new, paths) = apply_changes(t, &changes, &base)?;
    t.pk_of_item(&new)?;
    let size = item_size(&new).max(old.map_or(0, item_size));
    let old = t.items.insert(pk, new.clone());
    let attrs = match rv {
        "ALL_OLD" => old,
        "ALL_NEW" => Some(new),
        "UPDATED_OLD" => old.map(|o| project(&o, &paths)),
        "UPDATED_NEW" => Some(project(&new, &paths)),
        _ => None,
    };
    let mut out = json!({});
    if let Some(a) = attrs.filter(|a| !a.is_empty()) {
        out["Attributes"] = item_to_value(&a);
    }
    Ok(with_capacity(out, input, &name, units(size, 1024)))
}

pub fn delete(region: &mut Region, input: &Value) -> OpResult {
    let name = table_name(require_table_name(input)?).to_string();
    let rv = return_values(input, &["NONE", "ALL_OLD"])?;
    check_mixing(input)?;
    let mut b = Bindings::from_input(input)?;
    let cond = condition(input, &mut b)?;
    b.check_unused()?;
    let t = region.table_mut(&name)?;
    let key = item_from_value(required_item(input, "Key")?)?;
    let pk = t.pk_of_key(&key)?;
    let old = t.items.get(&pk);
    if !passes(&cond, old) {
        return Err(condition_failed(input, old));
    }
    let old = t.items.remove(&pk);
    let size = old.as_ref().map_or(0, item_size);
    let mut out = json!({});
    if rv == "ALL_OLD"
        && let Some(o) = old
    {
        out["Attributes"] = item_to_value(&o);
    }
    Ok(with_capacity(out, input, &name, units(size, 1024)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Select {
    All,
    Projected,
    Specific,
    Count,
}

impl Select {
    fn of(input: &Value, index: Option<&Index>, has_projection: bool) -> Result<Self, AwsError> {
        let param = if input.get("ProjectionExpression").is_some() {
            "ProjectionExpression"
        } else {
            "AttributesToGet"
        };
        let select = match input.str_field("Select") {
            None if has_projection => return Ok(Select::Specific),
            None if index.is_some() => return Ok(Select::Projected),
            None => return Ok(Select::All),
            Some("SPECIFIC_ATTRIBUTES") if has_projection => return Ok(Select::Specific),
            Some("SPECIFIC_ATTRIBUTES") => {
                return Err(validation(
                    "One or more parameter values were invalid: SPECIFIC_ATTRIBUTES requires ProjectionExpression or AttributesToGet",
                ));
            }
            Some("ALL_ATTRIBUTES") => Select::All,
            Some("ALL_PROJECTED_ATTRIBUTES") => Select::Projected,
            Some("COUNT") => Select::Count,
            Some(other) => {
                return Err(validation(format!(
                    "1 validation error detected: Value '{other}' at 'select' failed to satisfy constraint: Member must satisfy enum value set: [SPECIFIC_ATTRIBUTES, COUNT, ALL_ATTRIBUTES, ALL_PROJECTED_ATTRIBUTES]"
                )));
            }
        };
        let label = input.str_field("Select").unwrap_or_default();
        if has_projection {
            return Err(validation(format!(
                "One or more parameter values were invalid: Cannot specify the {param} when choosing to get {label}"
            )));
        }
        match (select, index) {
            (Select::All, Some(i)) if i.global && i.projection_type != "ALL" => {
                Err(validation(format!(
                    "One or more parameter values were invalid: Select type ALL_ATTRIBUTES is not supported for global secondary index {} because its projection type is not ALL",
                    i.name
                )))
            }
            (Select::Projected, None) => Err(validation(
                "One or more parameter values were invalid: Select type ALL_PROJECTED_ATTRIBUTES is not supported when no index is specified",
            )),
            _ => Ok(select),
        }
    }
}

type Row<'a> = (Option<KeyVal>, &'a Pk, &'a Item);

fn start_key(
    t: &Table,
    index: Option<&Index>,
    input: &Value,
    with_index_range: bool,
) -> Result<Option<(Option<KeyVal>, Pk)>, AwsError> {
    let Some(esk) = input.get("ExclusiveStartKey") else {
        return Ok(None);
    };
    let esk = item_from_value(esk)?;
    let bad = || {
        validation(
            "The provided starting key is invalid: The provided key element does not match the schema",
        )
    };
    let table_part: Item = t
        .key
        .names()
        .filter_map(|k| Some((k.clone(), esk.get(k)?.clone())))
        .collect();
    let pk = t.pk_of_key(&table_part).map_err(|_| bad())?;
    let range = match index
        .filter(|_| with_index_range)
        .and_then(|i| i.key.range.as_ref())
    {
        Some(r) => Some(esk.get(r).and_then(KeyVal::of).ok_or_else(bad)?),
        None => None,
    };
    Ok(Some((range, pk)))
}

fn row_cmp(row: &Row<'_>, start: &(Option<KeyVal>, Pk)) -> Ordering {
    (row.0.as_ref(), row.1).cmp(&(start.0.as_ref(), &start.1))
}

fn limit(input: &Value) -> Result<Option<usize>, AwsError> {
    match input.i64_field("Limit") {
        Some(l) if l < 1 => Err(validation(format!(
            "1 validation error detected: Value '{l}' at 'limit' failed to satisfy constraint: Member must have value greater than or equal to 1"
        ))),
        l => Ok(l.map(|l| l as usize)),
    }
}

struct Read<'a> {
    table: &'a Table,
    index: Option<&'a Index>,
    filter: Option<Cond>,
    projection: Option<Vec<Path>>,
    select: Select,
}

impl Read<'_> {
    fn view<'i>(&self, item: &'i Item) -> Cow<'i, Item> {
        match self.index {
            Some(i)
                if i.projection_type != "ALL"
                    && (i.global || (self.select != Select::All && self.projection.is_none())) =>
            {
                Cow::Owned(self.table.index_view(i, item))
            }
            _ => Cow::Borrowed(item),
        }
    }

    fn page(&self, rows: &[Row<'_>], input: &Value) -> OpResult {
        let limit = limit(input)?;
        let (mut scanned, mut bytes, mut count) = (0, 0, 0);
        let mut items = Vec::new();
        let mut last = None;
        for (_, _, item) in rows {
            if limit.is_some_and(|l| scanned >= l) || bytes >= PAGE_BYTES {
                break;
            }
            scanned += 1;
            bytes += item_size(item);
            last = Some(*item);
            let view = self.view(item);
            if self.filter.as_ref().is_none_or(|f| matches(f, &view)) {
                count += 1;
                if self.select != Select::Count {
                    items.push(match &self.projection {
                        Some(p) => item_to_value(&project(&view, p)),
                        None => item_to_value(&view),
                    });
                }
            }
        }
        let mut out = json!({ "Count": count, "ScannedCount": scanned });
        if self.select != Select::Count {
            out["Items"] = Value::Array(items);
        }
        if scanned < rows.len()
            && let Some(last) = last
        {
            out["LastEvaluatedKey"] = item_to_value(&self.table.key_attrs(last, self.index));
        }
        Ok(with_capacity(
            out,
            input,
            &self.table.name,
            read_capacity(bytes, input),
        ))
    }
}

fn consistent_on_gsi(input: &Value, index: Option<&Index>) -> Result<(), AwsError> {
    if index.is_some_and(|i| i.global)
        && input.get("ConsistentRead").and_then(Value::as_bool) == Some(true)
    {
        return Err(validation(
            "Consistent reads are not supported on global secondary indexes",
        ));
    }
    Ok(())
}

enum RangeKey {
    Cmp(CmpOp, KeyVal),
    Between(KeyVal, KeyVal),
    BeginsWith(KeyVal),
}

impl RangeKey {
    fn of(cond: &RangeCond) -> Option<Self> {
        Some(match cond {
            RangeCond::Cmp(op, v) => RangeKey::Cmp(*op, KeyVal::of(v)?),
            RangeCond::Between(lo, hi) => RangeKey::Between(KeyVal::of(lo)?, KeyVal::of(hi)?),
            RangeCond::BeginsWith(p) => RangeKey::BeginsWith(KeyVal::of(p)?),
        })
    }

    fn matches(&self, v: &KeyVal) -> bool {
        match self {
            RangeKey::Cmp(op, x) => {
                let o = v.cmp(x);
                match op {
                    CmpOp::Eq => o == Ordering::Equal,
                    CmpOp::Lt => o == Ordering::Less,
                    CmpOp::Le => o != Ordering::Greater,
                    CmpOp::Gt => o == Ordering::Greater,
                    CmpOp::Ge => o != Ordering::Less,
                    CmpOp::Ne => o != Ordering::Equal,
                }
            }
            RangeKey::Between(lo, hi) => v >= lo && v <= hi,
            RangeKey::BeginsWith(p) => match (v, p) {
                (KeyVal::S(v), KeyVal::S(p)) => v.starts_with(p.as_str()),
                (KeyVal::B(v), KeyVal::B(p)) => v.starts_with(p),
                _ => false,
            },
        }
    }
}

fn check_key_types(t: &Table, kc: &KeyCond) -> Result<(), AwsError> {
    let mismatch = || {
        validation(
            "One or more parameter values were invalid: Condition parameter type does not match schema type",
        )
    };
    if Some(kc.hash.1.type_name()) != t.attr_type(&kc.hash.0) {
        return Err(mismatch());
    }
    if let Some((name, cond)) = &kc.range {
        let want = t.attr_type(name);
        let values: Vec<&Attr> = match cond {
            RangeCond::Cmp(_, v) | RangeCond::BeginsWith(v) => vec![v],
            RangeCond::Between(a, b) => vec![a, b],
        };
        if let RangeCond::BeginsWith(v) = cond
            && (want == Some("N") || !matches!(v, Attr::S(_) | Attr::B(_)))
        {
            return Err(validation(format!(
                "Invalid KeyConditionExpression: Incorrect operand type for operator or function; operator or function: begins_with, operand type: {}",
                if want == Some("N") {
                    "NUMBER"
                } else {
                    v.long_type_name()
                }
            )));
        }
        if values.iter().any(|v| Some(v.type_name()) != want) {
            return Err(mismatch());
        }
    }
    Ok(())
}

pub fn query(region: &mut Region, input: &Value) -> OpResult {
    let t = region.table(require_table_name(input)?)?;
    check_mixing(input)?;
    let index = input
        .str_field("IndexName")
        .map(|n| t.index(n))
        .transpose()?;
    consistent_on_gsi(input, index)?;
    let key = index.map_or(&t.key, |i| &i.key);
    let mut b = Bindings::from_input(input)?;
    let parts = match (
        input.str_field("KeyConditionExpression"),
        input.obj_field("KeyConditions"),
    ) {
        (Some(e), _) => parse_key_condition(e, &mut b)?,
        (None, Some(m)) => legacy::key_conditions(m)?,
        (None, None) => {
            return Err(validation(
                "Either the KeyConditions or KeyConditionExpression parameter must be specified in the request.",
            ));
        }
    };
    let kc = KeyCond::resolve(parts, &key.hash, key.range.as_deref())?;
    check_key_types(t, &kc)?;
    let filter = filter(input, &mut b, "QueryFilter")?;
    let projection = projection(input, &mut b)?;
    b.check_unused()?;
    let select = Select::of(input, index, projection.is_some())?;
    let hash = KeyVal::of(&kc.hash.1).ok_or_else(|| validation("Invalid key value"))?;
    let mut rows: Vec<Row<'_>> = match index {
        None => t
            .items
            .range((hash.clone(), None)..)
            .take_while(|(pk, _)| pk.0 == hash)
            .map(|(pk, it)| (None, pk, it))
            .collect(),
        Some(i) => {
            let mut rows: Vec<Row<'_>> = t
                .items
                .iter()
                .filter(|(_, it)| {
                    t.in_index(i, it) && KeyVal::of(&it[&i.key.hash]).as_ref() == Some(&hash)
                })
                .map(|(pk, it)| {
                    (
                        i.key.range.as_ref().and_then(|r| KeyVal::of(&it[r])),
                        pk,
                        it,
                    )
                })
                .collect();
            rows.sort_by(|a, b| (a.0.as_ref(), a.1).cmp(&(b.0.as_ref(), b.1)));
            rows
        }
    };
    if let Some((_, cond)) = &kc.range {
        let range = RangeKey::of(cond).ok_or_else(|| validation("Invalid key value"))?;
        rows.retain(|(ir, pk, _)| {
            let v = if index.is_some() {
                ir.as_ref()
            } else {
                pk.1.as_ref()
            };
            v.is_some_and(|v| range.matches(v))
        });
    }
    let forward = input.get("ScanIndexForward").and_then(Value::as_bool) != Some(false);
    if !forward {
        rows.reverse();
    }
    let start = match start_key(t, index, input, true)? {
        Some(s) => rows
            .iter()
            .position(|r| {
                let o = row_cmp(r, &s);
                if forward {
                    o == Ordering::Greater
                } else {
                    o == Ordering::Less
                }
            })
            .unwrap_or(rows.len()),
        None => 0,
    };
    Read {
        table: t,
        index,
        filter,
        projection,
        select,
    }
    .page(&rows[start..], input)
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}

pub fn scan(region: &mut Region, input: &Value) -> OpResult {
    let t = region.table(require_table_name(input)?)?;
    check_mixing(input)?;
    let index = input
        .str_field("IndexName")
        .map(|n| t.index(n))
        .transpose()?;
    consistent_on_gsi(input, index)?;
    let segment = match (input.i64_field("Segment"), input.i64_field("TotalSegments")) {
        (None, None) => None,
        (Some(_), None) => {
            return Err(validation(
                "The TotalSegments parameter is required but was not present in the request when Segment parameter is present",
            ));
        }
        (None, Some(_)) => {
            return Err(validation(
                "The Segment parameter is required but was not present in the request when TotalSegments parameter is present",
            ));
        }
        (Some(s), Some(n)) => {
            if !(1..=1_000_000).contains(&n) {
                return Err(validation(format!(
                    "1 validation error detected: Value '{n}' at 'totalSegments' failed to satisfy constraint: Member must have value less than or equal to 1000000"
                )));
            }
            if s < 0 || s >= n {
                return Err(validation(format!(
                    "The Segment parameter is zero-based and must be less than parameter TotalSegments: Segment: {s} is not less than TotalSegments: {n}"
                )));
            }
            Some((s as u64, n as u64))
        }
    };
    let mut b = Bindings::from_input(input)?;
    let filter = filter(input, &mut b, "ScanFilter")?;
    let projection = projection(input, &mut b)?;
    b.check_unused()?;
    let select = Select::of(input, index, projection.is_some())?;
    let rows: Vec<Row<'_>> = t
        .items
        .iter()
        .filter(|(_, it)| index.is_none_or(|i| t.in_index(i, it)))
        .filter(|(pk, _)| segment.is_none_or(|(s, n)| fnv(&pk.0.bytes()) % n == s))
        .map(|(pk, it)| (None, pk, it))
        .collect();
    let start = match start_key(t, index, input, false)? {
        Some(s) => rows
            .iter()
            .position(|r| row_cmp(r, &s) == Ordering::Greater)
            .unwrap_or(rows.len()),
        None => 0,
    };
    Read {
        table: t,
        index,
        filter,
        projection,
        select,
    }
    .page(&rows[start..], input)
}

fn request_items(input: &Value) -> Result<&Map<String, Value>, AwsError> {
    input
        .obj_field("RequestItems")
        .filter(|m| !m.is_empty())
        .ok_or_else(|| {
            validation(
                "1 validation error detected: Value null at 'requestItems' failed to satisfy constraint: Member must have length greater than or equal to 1",
            )
        })
}

fn duplicates() -> AwsError {
    validation("Provided list of item keys contains duplicates")
}

pub fn batch_get(region: &mut Region, input: &Value) -> OpResult {
    let req = request_items(input)?;
    let total: usize = req.values().map(|s| s.array_field("Keys").len()).sum();
    if total > 100 {
        return Err(validation(
            "Too many items requested for the BatchGetItem call",
        ));
    }
    let mut responses = Map::new();
    for (name, spec) in req {
        let t = region.table(name)?;
        check_mixing(spec)?;
        let mut b = Bindings::from_input(spec)?;
        let proj = projection(spec, &mut b)?;
        b.check_unused()?;
        let mut seen = BTreeSet::new();
        let mut found = Vec::new();
        for k in spec.array_field("Keys") {
            let pk = t.pk_of_key(&item_from_value(k)?)?;
            if !seen.insert(pk.clone()) {
                return Err(duplicates());
            }
            if let Some(it) = t.items.get(&pk) {
                found.push(item_to_value(&match &proj {
                    Some(p) => project(it, p),
                    None => it.clone(),
                }));
            }
        }
        responses.insert(name.clone(), Value::Array(found));
    }
    Ok(json!({ "Responses": responses, "UnprocessedKeys": {} }))
}

pub fn batch_write(region: &mut Region, input: &Value) -> OpResult {
    let req = request_items(input)?;
    let total: usize = req.values().map(|v| v.as_array().map_or(0, Vec::len)).sum();
    if total > 25 {
        return Err(validation(
            "Too many items requested for the BatchWriteItem call",
        ));
    }
    let mut plan: Vec<(String, Pk, Option<Item>)> = Vec::new();
    for (name, requests) in req {
        let t = region.table(name)?;
        let mut seen = BTreeSet::new();
        for r in requests.as_array().map(Vec::as_slice).unwrap_or_default() {
            let (pk, item) = match (r.get("PutRequest"), r.get("DeleteRequest")) {
                (Some(p), None) => {
                    let item = item_from_value(required_item(p, "Item")?)?;
                    (t.pk_of_item(&item)?, Some(item))
                }
                (None, Some(d)) => (
                    t.pk_of_key(&item_from_value(required_item(d, "Key")?)?)?,
                    None,
                ),
                _ => {
                    return Err(validation(
                        "Supplied AttributeValue has more than one datatypes set, must contain exactly one of the supported datatypes",
                    ));
                }
            };
            if !seen.insert(pk.clone()) {
                return Err(duplicates());
            }
            plan.push((table_name(name).to_string(), pk, item));
        }
    }
    for (name, pk, item) in plan {
        let t = region.table_mut(&name)?;
        match item {
            Some(it) => {
                t.items.insert(pk, it);
            }
            None => {
                t.items.remove(&pk);
            }
        }
    }
    Ok(json!({ "UnprocessedItems": {} }))
}

fn transact_items(input: &Value) -> Result<&[Value], AwsError> {
    let items = input.array_field("TransactItems");
    if items.is_empty() {
        return Err(validation(
            "1 validation error detected: Value '[]' at 'transactItems' failed to satisfy constraint: Member must have length greater than or equal to 1",
        ));
    }
    if items.len() > 100 {
        return Err(validation(
            "1 validation error detected: Value at 'transactItems' failed to satisfy constraint: Member must have length less than or equal to 100",
        ));
    }
    Ok(items)
}

pub fn transact_get(region: &mut Region, input: &Value) -> OpResult {
    let mut responses = Vec::new();
    for entry in transact_items(input)? {
        let get = required_item(entry, "Get")?;
        let t = region.table(require_table_name(get)?)?;
        let mut b = Bindings::from_input(get)?;
        let proj = projection(get, &mut b)?;
        b.check_unused()?;
        let pk = t.pk_of_key(&item_from_value(required_item(get, "Key")?)?)?;
        responses.push(match t.items.get(&pk) {
            Some(it) => json!({ "Item": item_to_value(&match &proj {
                Some(p) => project(it, p),
                None => it.clone(),
            }) }),
            None => json!({}),
        });
    }
    Ok(json!({ "Responses": responses }))
}

fn reason(code: &str, message: Option<&str>, item: Option<&Item>) -> Value {
    let mut r = json!({ "Code": code });
    if let Some(m) = message {
        r["Message"] = json!(m);
    }
    if let Some(it) = item {
        r["Item"] = item_to_value(it);
    }
    r
}

pub fn transact_write(region: &mut Region, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let now = ctx.now_ms();
    region
        .tx_tokens
        .retain(|_, (_, at)| now - *at < TOKEN_WINDOW_MS);
    let token = input.str_field("ClientRequestToken").map(str::to_string);
    let mut fingerprint = input.clone();
    if let Some(o) = fingerprint.as_object_mut() {
        o.remove("ClientRequestToken");
    }
    if let Some(tok) = &token
        && let Some((seen, _)) = region.tx_tokens.get(tok)
    {
        if *seen == fingerprint {
            return Ok(json!({}));
        }
        return Err(err(
            "IdempotentParameterMismatchException",
            "Specified idempotency token was used with different request parameters within the 10-minute window",
        ));
    }
    let entries = transact_items(input)?;
    let mut targets = BTreeSet::new();
    let mut plan: Vec<(String, Pk, Option<Option<Item>>)> = Vec::new();
    let mut reasons = Vec::new();
    let mut failed = false;
    for entry in entries {
        let kinds: Vec<&str> = ["ConditionCheck", "Put", "Delete", "Update"]
            .into_iter()
            .filter(|k| entry.get(*k).is_some())
            .collect();
        let [kind] = kinds.as_slice() else {
            return Err(validation(
                "TransactItems can only contain one of Check, Put, Update or Delete",
            ));
        };
        let spec = &entry[*kind];
        let name = table_name(require_table_name(spec)?).to_string();
        let t = region.table(&name)?;
        let mut b = Bindings::from_input(spec)?;
        let cond = match spec.str_field("ConditionExpression") {
            Some(e) => Some(parse_condition("ConditionExpression", e, &mut b)?),
            None if *kind == "ConditionCheck" => {
                return Err(validation(
                    "The ConditionExpression parameter is required for ConditionCheck",
                ));
            }
            None => None,
        };
        let (pk, put, update) = match *kind {
            "Put" => {
                let item = item_from_value(required_item(spec, "Item")?)?;
                (t.pk_of_item(&item)?, Some(item), None)
            }
            "Update" => {
                let key = item_from_value(required_item(spec, "Key")?)?;
                let e = spec.str_field("UpdateExpression").ok_or_else(|| {
                    validation(
                        "1 validation error detected: Value null at 'transactItems.member.update.updateExpression' failed to satisfy constraint: Member must not be null",
                    )
                })?;
                let u = parse_update(e, &mut b)?;
                (t.pk_of_key(&key)?, None, Some((key, u)))
            }
            _ => (
                t.pk_of_key(&item_from_value(required_item(spec, "Key")?)?)?,
                None,
                None,
            ),
        };
        b.check_unused()?;
        if !targets.insert((name.clone(), pk.clone())) {
            return Err(validation(
                "Transaction request cannot include multiple operations on one item",
            ));
        }
        let current = t.items.get(&pk);
        if !passes(&cond, current) {
            failed = true;
            let item = current.filter(|_| {
                spec.str_field("ReturnValuesOnConditionCheckFailure") == Some("ALL_OLD")
            });
            reasons.push(reason(
                "ConditionalCheckFailed",
                Some("The conditional request failed"),
                item,
            ));
            continue;
        }
        let change = match (*kind, put, update) {
            ("Put", Some(item), _) => Some(Some(item)),
            ("Delete", ..) => Some(None),
            ("Update", _, Some((key, u))) => {
                let base = current.cloned().unwrap_or(key);
                let applied = check_key_untouched(t, &u.targets())
                    .and_then(|_| apply(&u, &base))
                    .and_then(|new| t.pk_of_item(&new).map(|_| new));
                match applied {
                    Ok(new) => Some(Some(new)),
                    Err(e) => {
                        failed = true;
                        reasons.push(reason("ValidationError", Some(&e.message), None));
                        continue;
                    }
                }
            }
            _ => None,
        };
        reasons.push(reason("None", None, None));
        plan.push((name, pk, change));
    }
    if failed {
        let codes: Vec<&str> = reasons
            .iter()
            .map(|r| r["Code"].as_str().unwrap_or_default())
            .collect();
        let mut e = err(
            "TransactionCanceledException",
            format!(
                "Transaction cancelled, please refer cancellation reasons for specific reasons [{}]",
                codes.join(", ")
            ),
        );
        e.data
            .insert("CancellationReasons".into(), Value::Array(reasons));
        return Err(e);
    }
    for (name, pk, change) in plan {
        let t = region.table_mut(&name)?;
        match change {
            Some(Some(item)) => {
                t.items.insert(pk, item);
            }
            Some(None) => {
                t.items.remove(&pk);
            }
            None => {}
        }
    }
    if let Some(tok) = token {
        region.tx_tokens.insert(tok, (fingerprint, now));
    }
    Ok(json!({}))
}

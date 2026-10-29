use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use super::versions::{publish, resolve};
use super::{
    Function, MapExt, Region, conflict, err, function_arn, invalid, no_function, not_found,
    now_iso, page, sha256_b64, split_name, target,
};
use crate::codec::AwsError;
use crate::service::{Ctx, InputExt};

const CONFIG_MEMBERS: &[&str] = &[
    "Role",
    "Handler",
    "Description",
    "Timeout",
    "MemorySize",
    "Runtime",
    "KMSKeyArn",
    "DeadLetterConfig",
    "TracingConfig",
    "EphemeralStorage",
    "FileSystemConfigs",
];

fn code_of(input: &Value) -> (usize, String) {
    if let Some(zip) = input
        .str_field("ZipFile")
        .and_then(|z| STANDARD.decode(z).ok())
    {
        return (zip.len(), sha256_b64(&zip));
    }
    let source = ["S3Bucket", "S3Key", "S3ObjectVersion", "ImageUri"]
        .iter()
        .filter_map(|k| input.str_field(k))
        .collect::<Vec<_>>()
        .join("/");
    (0, sha256_b64(source.as_bytes()))
}

fn layers(region: &Region, input: &Value) -> Option<Value> {
    input.get("Layers").map(|l| {
        Value::Array(
            l.as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|arn| {
                    let size = super::layers::version_size(region, arn).unwrap_or(0);
                    json!({ "Arn": arn, "CodeSize": size })
                })
                .collect(),
        )
    })
}

fn apply_config(config: &mut Map<String, Value>, region: &Region, input: &Value) {
    for key in CONFIG_MEMBERS {
        if let Some(v) = input.get(*key).filter(|v| !v.is_null()) {
            config.insert(key.to_string(), v.clone());
        }
    }
    if let Some(env) = input.obj_field("Environment") {
        let vars = env.get("Variables").cloned().unwrap_or_else(|| json!({}));
        config.insert("Environment".into(), json!({ "Variables": vars }));
    }
    if let Some(vpc) = input.obj_field("VpcConfig") {
        let mut v = vpc.clone();
        v.entry("SubnetIds").or_insert_with(|| json!([]));
        v.entry("SecurityGroupIds").or_insert_with(|| json!([]));
        v.insert("VpcId".into(), json!("vpc-wasmo"));
        config.insert("VpcConfig".into(), Value::Object(v));
    }
    if let Some(l) = layers(region, input) {
        config.insert("Layers".into(), l);
    }
    if let Some(img) = input.obj_field("ImageConfig") {
        config.insert("ImageConfigResponse".into(), json!({ "ImageConfig": img }));
    }
    if let Some(snap) = input.obj_field("SnapStart") {
        config.insert(
            "SnapStart".into(),
            json!({
                "ApplyOn": snap.get("ApplyOn").cloned().unwrap_or(json!("None")),
                "OptimizationStatus": "Off",
            }),
        );
    }
    if let Some(logging) = input.obj_field("LoggingConfig") {
        let mut l = config
            .get("LoggingConfig")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        l.extend(logging.clone());
        config.insert("LoggingConfig".into(), Value::Object(l));
    }
}

fn check_revision(config: &Map<String, Value>, input: &Value) -> Result<(), AwsError> {
    match input.str_field("RevisionId") {
        Some(r) if Some(r) != config.str_field("RevisionId") => Err(err(
            412,
            "PreconditionFailedException",
            "The Revision Id provided does not match the latest Revision Id. Call the GetFunction/GetAlias API to retrieve the latest Revision Id",
        )),
        _ => Ok(()),
    }
}

fn touch(ctx: &mut Ctx<'_>, config: &mut Map<String, Value>) {
    config.insert("LastModified".into(), json!(now_iso(ctx)));
    config.insert("RevisionId".into(), json!(ctx.uuid()));
}

fn create(ctx: &mut Ctx<'_>, region: &mut Region, input: &Value) -> Result<Value, AwsError> {
    let (name, _) = split_name(input.require_str("FunctionName")?);
    if region.functions.contains_key(&name) {
        return Err(conflict(format!("Function already exist: {name}")));
    }
    let role = input.require_str("Role")?;
    let package = input.str_field("PackageType").unwrap_or("Zip");
    if package == "Zip" && input.str_field("Runtime").is_none() {
        return Err(invalid(
            "The Runtime parameter is required for Zip package type functions.",
        ));
    }
    let code = input.get("Code").cloned().unwrap_or_else(|| json!({}));
    let (size, sha) = code_of(&code);
    let arn = function_arn(ctx, &name);
    let mut config = Map::new();
    for (k, v) in [
        ("FunctionName", json!(name)),
        ("FunctionArn", json!(arn)),
        ("Role", json!(role)),
        ("CodeSize", json!(size)),
        ("Description", json!("")),
        ("Timeout", json!(3)),
        ("MemorySize", json!(128)),
        ("CodeSha256", json!(sha)),
        ("Version", json!("$LATEST")),
        ("TracingConfig", json!({ "Mode": "PassThrough" })),
        ("State", json!("Active")),
        ("LastUpdateStatus", json!("Successful")),
        ("PackageType", json!(package)),
        (
            "Architectures",
            input
                .get("Architectures")
                .cloned()
                .unwrap_or(json!(["x86_64"])),
        ),
        ("EphemeralStorage", json!({ "Size": 512 })),
        (
            "SnapStart",
            json!({ "ApplyOn": "None", "OptimizationStatus": "Off" }),
        ),
        (
            "LoggingConfig",
            json!({ "LogFormat": "Text", "LogGroup": format!("/aws/lambda/{name}") }),
        ),
    ] {
        config.insert(k.to_string(), v);
    }
    if let Some(uri) = code.str_field("ImageUri") {
        config.insert("ImageUri".into(), json!(uri));
    }
    apply_config(&mut config, region, input);
    if let Some(runtime) = config.str_field("Runtime") {
        let runtime_arn = format!(
            "arn:aws:lambda:{}::runtime:{}",
            ctx.region,
            sha256_hex(runtime)
        );
        config.insert(
            "RuntimeVersionConfig".into(),
            json!({ "RuntimeVersionArn": runtime_arn }),
        );
    }
    touch(ctx, &mut config);
    let mut f = Function {
        config,
        ..Function::default()
    };
    f.code_signing = input.str_field("CodeSigningConfigArn").map(str::to_string);
    let tags = super::string_map(input.obj_field("Tags"));
    if !tags.is_empty() {
        region.tags.insert(arn, tags);
    }
    let mut out = f.config.clone();
    if input.get("Publish").and_then(Value::as_bool) == Some(true) {
        out = publish(ctx, &mut f, None);
    }
    region.functions.insert(name, f);
    out.insert("State".into(), json!("Pending"));
    out.insert(
        "StateReason".into(),
        json!("The function is being created."),
    );
    out.insert("StateReasonCode".into(), json!("Creating"));
    Ok(Value::Object(out))
}

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn function<'a>(
    ctx: &Ctx<'_>,
    region: &'a mut Region,
    input: &Value,
) -> Result<(&'a mut Function, Option<String>), AwsError> {
    let (name, qualifier) = target(input)?;
    let f = region
        .functions
        .get_mut(&name)
        .ok_or_else(|| no_function(ctx, &name, None))?;
    Ok((f, qualifier))
}

fn update_code(ctx: &mut Ctx<'_>, region: &mut Region, input: &Value) -> Result<Value, AwsError> {
    let (f, _) = function(ctx, region, input)?;
    check_revision(&f.config, input)?;
    let (size, sha) = code_of(input);
    if input.get("DryRun").and_then(Value::as_bool) == Some(true) {
        let mut out = f.config.clone();
        out.insert("CodeSize".into(), json!(size));
        out.insert("CodeSha256".into(), json!(sha));
        return Ok(Value::Object(out));
    }
    f.config.insert("CodeSize".into(), json!(size));
    f.config.insert("CodeSha256".into(), json!(sha));
    if let Some(a) = input.get("Architectures") {
        f.config.insert("Architectures".into(), a.clone());
    }
    touch(ctx, &mut f.config);
    if input.get("Publish").and_then(Value::as_bool) == Some(true) {
        return Ok(Value::Object(publish(ctx, f, None)));
    }
    Ok(Value::Object(f.config.clone()))
}

fn update_config(ctx: &mut Ctx<'_>, region: &mut Region, input: &Value) -> Result<Value, AwsError> {
    let mut config = function(ctx, region, input)?.0.config.clone();
    check_revision(&config, input)?;
    apply_config(&mut config, region, input);
    touch(ctx, &mut config);
    let (f, _) = function(ctx, region, input)?;
    f.config = config;
    Ok(Value::Object(f.config.clone()))
}

fn get(ctx: &Ctx<'_>, region: &mut Region, input: &Value) -> Result<Value, AwsError> {
    let (name, qualifier) = target(input)?;
    let f = region
        .functions
        .get(&name)
        .ok_or_else(|| no_function(ctx, &name, qualifier.as_deref()))?;
    let config = resolve(ctx, f, qualifier.as_deref())?;
    let mut out = json!({
        "Configuration": config,
        "Code": {
            "RepositoryType": "S3",
            "Location": format!("{}/wasmo-lambda-code/{name}", ctx.endpoint),
        },
    });
    if let Some(uri) = config.str_field("ImageUri") {
        out["Code"] = json!({ "RepositoryType": "ECR", "ImageUri": uri, "ResolvedImageUri": uri });
    }
    if let Some(r) = f.reserved {
        out["Concurrency"] = json!({ "ReservedConcurrentExecutions": r });
    }
    if let Some(tags) = region.tags.get(f.arn()).filter(|t| !t.is_empty()) {
        out["Tags"] = json!(tags);
    }
    Ok(out)
}

fn list(region: &Region, input: &Value) -> Value {
    let all = input.str_field("FunctionVersion") == Some("ALL");
    let mut items = Vec::new();
    for f in region.functions.values() {
        items.push(Value::Object(f.config.clone()));
        if all {
            items.extend(f.versions.values().cloned().map(Value::Object));
        }
    }
    let (items, next) = page(items, input, "MaxItems");
    let mut out = json!({ "Functions": items });
    if let Some(n) = next {
        out["NextMarker"] = json!(n);
    }
    out
}

fn delete(ctx: &Ctx<'_>, region: &mut Region, input: &Value) -> Result<Value, AwsError> {
    let (name, qualifier) = target(input)?;
    let f = region
        .functions
        .get_mut(&name)
        .ok_or_else(|| no_function(ctx, &name, None))?;
    match qualifier.as_deref() {
        None => {
            let arn = f.arn().to_string();
            region.functions.remove(&name);
            region.tags.remove(&arn);
        }
        Some("$LATEST") => {
            return Err(invalid(
                "$LATEST version cannot be deleted without deleting the function.",
            ));
        }
        Some(q) => {
            let n: u64 = q
                .parse()
                .map_err(|_| invalid("Deletion of aliases is not supported."))?;
            if let Some(alias) = f
                .aliases
                .values()
                .find(|a| a.str_field("FunctionVersion") == Some(q))
            {
                return Err(conflict(format!(
                    "Unable to delete version because the following aliases reference it: [{}]",
                    alias.str_field("Name").unwrap_or_default()
                )));
            }
            f.versions.remove(&n);
        }
    }
    Ok(json!({}))
}

pub(super) fn call(
    ctx: &mut Ctx<'_>,
    region: &mut Region,
    operation: &str,
    input: &Value,
) -> Result<Option<Value>, AwsError> {
    Ok(Some(match operation {
        "CreateFunction" => create(ctx, region, input)?,
        "GetFunction" => get(ctx, region, input)?,
        "GetFunctionConfiguration" => {
            let (name, qualifier) = target(input)?;
            let f = region
                .functions
                .get(&name)
                .ok_or_else(|| no_function(ctx, &name, qualifier.as_deref()))?;
            Value::Object(resolve(ctx, f, qualifier.as_deref())?)
        }
        "ListFunctions" => list(region, input),
        "DeleteFunction" => delete(ctx, region, input)?,
        "UpdateFunctionCode" => update_code(ctx, region, input)?,
        "UpdateFunctionConfiguration" => update_config(ctx, region, input)?,
        "PutFunctionConcurrency" => {
            let (f, _) = function(ctx, region, input)?;
            let n = input
                .i64_field("ReservedConcurrentExecutions")
                .ok_or_else(|| invalid("ReservedConcurrentExecutions is required."))?;
            if !(0..=1000).contains(&n) {
                return Err(invalid(
                    "Specified ReservedConcurrentExecutions for function decreases account's UnreservedConcurrentExecution below its minimum value of [100].",
                ));
            }
            f.reserved = Some(n);
            json!({ "ReservedConcurrentExecutions": n })
        }
        "GetFunctionConcurrency" => {
            let (f, _) = function(ctx, region, input)?;
            match f.reserved {
                Some(n) => json!({ "ReservedConcurrentExecutions": n }),
                None => json!({}),
            }
        }
        "DeleteFunctionConcurrency" => {
            function(ctx, region, input)?.0.reserved = None;
            json!({})
        }
        _ => return Ok(None),
    }))
}

fn url_config(
    ctx: &mut Ctx<'_>,
    f: &Function,
    qualifier: Option<&str>,
    input: &Value,
) -> Map<String, Value> {
    let now = now_iso(ctx);
    let mut id = ctx.uuid().replace('-', "");
    id.truncate(32);
    let arn = match qualifier {
        Some(q) => format!("{}:{q}", f.arn()),
        None => f.arn().to_string(),
    };
    let mut c = Map::new();
    c.insert(
        "FunctionUrl".into(),
        json!(format!("https://{id}.lambda-url.{}.on.aws/", ctx.region)),
    );
    c.insert("FunctionArn".into(), json!(arn));
    c.insert("CreationTime".into(), json!(now));
    c.insert("LastModifiedTime".into(), json!(now));
    c.insert("InvokeMode".into(), json!("BUFFERED"));
    for k in ["AuthType", "Cors", "InvokeMode"] {
        if let Some(v) = input.get(k).filter(|v| !v.is_null()) {
            c.insert(k.into(), v.clone());
        }
    }
    c
}

fn qualifier_key(qualifier: Option<&str>) -> String {
    qualifier.unwrap_or("$LATEST").to_string()
}

pub(super) fn settings(
    ctx: &mut Ctx<'_>,
    region: &mut Region,
    operation: &str,
    input: &Value,
) -> Result<Option<Value>, AwsError> {
    Ok(Some(match operation {
        "CreateFunctionUrlConfig" => {
            let (f, q) = function(ctx, region, input)?;
            let key = qualifier_key(q.as_deref());
            if f.urls.contains_key(&key) {
                return Err(conflict(format!(
                    "Failed to create function url config for [functionArn = {}]. Error message:  FunctionUrlConfig exists for this Lambda function",
                    f.arn()
                )));
            }
            let c = url_config(ctx, f, q.as_deref(), input);
            f.urls.insert(key, c.clone());
            Value::Object(c)
        }
        "GetFunctionUrlConfig" | "UpdateFunctionUrlConfig" | "DeleteFunctionUrlConfig" => {
            let (f, q) = function(ctx, region, input)?;
            let key = qualifier_key(q.as_deref());
            let arn = f.arn().to_string();
            let Some(c) = f.urls.get_mut(&key) else {
                return Err(not_found(format!(
                    "The resource you requested does not exist. (Service: Lambda, Status Code: 404, Request ID: {arn})"
                )));
            };
            match operation {
                "DeleteFunctionUrlConfig" => {
                    f.urls.remove(&key);
                    json!({})
                }
                "UpdateFunctionUrlConfig" => {
                    for k in ["AuthType", "Cors", "InvokeMode"] {
                        if let Some(v) = input.get(k).filter(|v| !v.is_null()) {
                            c.insert(k.into(), v.clone());
                        }
                    }
                    c.insert("LastModifiedTime".into(), json!(now_iso(ctx)));
                    Value::Object(c.clone())
                }
                _ => Value::Object(c.clone()),
            }
        }
        "ListFunctionUrlConfigs" => {
            let (f, _) = function(ctx, region, input)?;
            let items: Vec<Value> = f.urls.values().cloned().map(Value::Object).collect();
            let (items, next) = page(items, input, "MaxItems");
            let mut out = json!({ "FunctionUrlConfigs": items });
            if let Some(n) = next {
                out["NextMarker"] = json!(n);
            }
            out
        }
        "PutProvisionedConcurrencyConfig" => {
            let (f, q) = function(ctx, region, input)?;
            let q = q.ok_or_else(|| {
                invalid("Provisioned Concurrency Configs cannot be applied to unpublished function versions.")
            })?;
            let n = input
                .i64_field("ProvisionedConcurrentExecutions")
                .unwrap_or(0);
            let mut c = Map::new();
            c.insert("RequestedProvisionedConcurrentExecutions".into(), json!(n));
            c.insert("AvailableProvisionedConcurrentExecutions".into(), json!(n));
            c.insert("AllocatedProvisionedConcurrentExecutions".into(), json!(n));
            c.insert("Status".into(), json!("READY"));
            c.insert("LastModified".into(), json!(now_iso(ctx)));
            c.insert("FunctionArn".into(), json!(format!("{}:{q}", f.arn())));
            f.provisioned.insert(q, c.clone());
            Value::Object(c)
        }
        "GetProvisionedConcurrencyConfig" | "DeleteProvisionedConcurrencyConfig" => {
            let (f, q) = function(ctx, region, input)?;
            let q = q.unwrap_or_default();
            if operation == "DeleteProvisionedConcurrencyConfig" {
                f.provisioned.remove(&q);
                json!({})
            } else {
                Value::Object(f.provisioned.get(&q).cloned().ok_or_else(|| {
                    err(
                        404,
                        "ProvisionedConcurrencyConfigNotFoundException",
                        "No Provisioned Concurrency Config found for this function",
                    )
                })?)
            }
        }
        "ListProvisionedConcurrencyConfigs" => {
            let (f, _) = function(ctx, region, input)?;
            let items: Vec<Value> = f.provisioned.values().cloned().map(Value::Object).collect();
            json!({ "ProvisionedConcurrencyConfigs": items })
        }
        "PutFunctionRecursionConfig" => {
            let (f, _) = function(ctx, region, input)?;
            let v = input.require_str("RecursiveLoop")?.to_string();
            f.recursion = Some(v.clone());
            json!({ "RecursiveLoop": v })
        }
        "GetFunctionRecursionConfig" => {
            let (f, _) = function(ctx, region, input)?;
            json!({ "RecursiveLoop": f.recursion.as_deref().unwrap_or("Terminate") })
        }
        "PutRuntimeManagementConfig" => {
            let (f, _) = function(ctx, region, input)?;
            let mut c = Map::new();
            c.insert(
                "UpdateRuntimeOn".into(),
                json!(input.require_str("UpdateRuntimeOn")?),
            );
            c.insert("FunctionArn".into(), json!(f.arn()));
            if let Some(a) = input.str_field("RuntimeVersionArn") {
                c.insert("RuntimeVersionArn".into(), json!(a));
            }
            f.runtime_management = Some(c.clone());
            Value::Object(c)
        }
        "GetRuntimeManagementConfig" => {
            let (f, _) = function(ctx, region, input)?;
            let mut c = f.runtime_management.clone().unwrap_or_default();
            c.entry("UpdateRuntimeOn").or_insert(json!("Auto"));
            c.insert("FunctionArn".into(), json!(f.arn()));
            Value::Object(c)
        }
        "PutFunctionScalingConfig" | "GetFunctionScalingConfig" => {
            let (f, q) = function(ctx, region, input)?;
            let key = qualifier_key(q.as_deref());
            if operation == "PutFunctionScalingConfig" {
                let c = input
                    .get("FunctionScalingConfig")
                    .cloned()
                    .unwrap_or(json!({}));
                f.scaling.insert(key, c);
                json!({ "FunctionState": "Active" })
            } else {
                let c = f.scaling.get(&key).cloned().unwrap_or(json!({}));
                json!({
                    "FunctionArn": f.arn(),
                    "AppliedFunctionScalingConfig": c,
                    "RequestedFunctionScalingConfig": c,
                })
            }
        }
        "GetAccountSettings" => {
            let reserved: i64 = region.functions.values().filter_map(|f| f.reserved).sum();
            let code: i64 = region
                .functions
                .values()
                .filter_map(|f| f.config.get("CodeSize").and_then(Value::as_i64))
                .sum();
            json!({
                "AccountLimit": {
                    "TotalCodeSize": 80530636800_i64,
                    "CodeSizeUnzipped": 262144000,
                    "CodeSizeZipped": 52428800,
                    "ConcurrentExecutions": 1000,
                    "UnreservedConcurrentExecutions": 1000 - reserved,
                },
                "AccountUsage": {
                    "TotalCodeSize": code,
                    "FunctionCount": region.functions.len(),
                },
            })
        }
        _ => return signing(ctx, region, operation, input),
    }))
}

fn signing(
    ctx: &mut Ctx<'_>,
    region: &mut Region,
    operation: &str,
    input: &Value,
) -> Result<Option<Value>, AwsError> {
    let missing = |arn: &str| {
        err(
            404,
            "CodeSigningConfigNotFoundException",
            format!(
                "The code signing configuration cannot be found. Check that the provided configuration is not deleted: {arn}."
            ),
        )
    };
    Ok(Some(match operation {
        "CreateCodeSigningConfig" => {
            let mut id = ctx.uuid().replace('-', "");
            id.truncate(17);
            let id = format!("csc-{id}");
            let arn = ctx.arn("lambda", &format!("code-signing-config:{id}"));
            let mut c = Map::new();
            c.insert("CodeSigningConfigId".into(), json!(id));
            c.insert("CodeSigningConfigArn".into(), json!(arn));
            c.insert(
                "Description".into(),
                json!(input.str_field("Description").unwrap_or_default()),
            );
            c.insert(
                "AllowedPublishers".into(),
                input.get("AllowedPublishers").cloned().unwrap_or(json!({})),
            );
            c.insert(
                "CodeSigningPolicies".into(),
                input
                    .get("CodeSigningPolicies")
                    .cloned()
                    .unwrap_or(json!({ "UntrustedArtifactOnDeployment": "Warn" })),
            );
            c.insert("LastModified".into(), json!(now_iso(ctx)));
            let tags = super::string_map(input.obj_field("Tags"));
            if !tags.is_empty() {
                region.tags.insert(arn.clone(), tags);
            }
            region.signing_configs.insert(arn, c.clone());
            json!({ "CodeSigningConfig": c })
        }
        "GetCodeSigningConfig" | "UpdateCodeSigningConfig" | "DeleteCodeSigningConfig" => {
            let arn = input.require_str("CodeSigningConfigArn")?.to_string();
            let c = region
                .signing_configs
                .get_mut(&arn)
                .ok_or_else(|| missing(&arn))?;
            match operation {
                "DeleteCodeSigningConfig" => {
                    region.signing_configs.remove(&arn);
                    json!({})
                }
                "UpdateCodeSigningConfig" => {
                    for k in ["Description", "AllowedPublishers", "CodeSigningPolicies"] {
                        if let Some(v) = input.get(k).filter(|v| !v.is_null()) {
                            c.insert(k.into(), v.clone());
                        }
                    }
                    c.insert("LastModified".into(), json!(now_iso(ctx)));
                    json!({ "CodeSigningConfig": c })
                }
                _ => json!({ "CodeSigningConfig": c }),
            }
        }
        "ListCodeSigningConfigs" => {
            let items: Vec<Value> = region
                .signing_configs
                .values()
                .cloned()
                .map(Value::Object)
                .collect();
            json!({ "CodeSigningConfigs": items })
        }
        "ListFunctionsByCodeSigningConfig" => {
            let arn = input.require_str("CodeSigningConfigArn")?;
            if !region.signing_configs.contains_key(arn) {
                return Err(missing(arn));
            }
            let arns: Vec<&str> = region
                .functions
                .values()
                .filter(|f| f.code_signing.as_deref() == Some(arn))
                .map(Function::arn)
                .collect();
            json!({ "FunctionArns": arns })
        }
        "PutFunctionCodeSigningConfig" => {
            let arn = input.require_str("CodeSigningConfigArn")?.to_string();
            if !region.signing_configs.contains_key(&arn) {
                return Err(missing(&arn));
            }
            let (f, _) = function(ctx, region, input)?;
            f.code_signing = Some(arn.clone());
            json!({ "CodeSigningConfigArn": arn, "FunctionName": f.name() })
        }
        "GetFunctionCodeSigningConfig" => {
            let (f, _) = function(ctx, region, input)?;
            json!({
                "CodeSigningConfigArn": f.code_signing.clone().unwrap_or_default(),
                "FunctionName": f.name(),
            })
        }
        "DeleteFunctionCodeSigningConfig" => {
            function(ctx, region, input)?.0.code_signing = None;
            json!({})
        }
        _ => return Ok(None),
    }))
}

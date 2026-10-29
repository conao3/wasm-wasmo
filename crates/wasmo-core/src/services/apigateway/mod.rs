mod account;
mod deploy;
pub(crate) mod invoke;
pub(crate) mod proxy;
mod rest;
pub(crate) mod store;
pub(crate) mod vtl;

use std::collections::{BTreeMap, HashMap};

use indexmap::IndexMap;
use serde_json::Value;

use crate::codec::AwsError;
use crate::embedded_model;
use crate::service::{Ctx, OpResult, Service, ServiceModel};
use crate::wire::{Request, Response};
use store::Obj;

#[derive(Debug, Clone, Default)]
struct Integration {
    info: Obj,
    responses: BTreeMap<String, Obj>,
}

#[derive(Debug, Clone, Default)]
struct Method {
    info: Obj,
    responses: BTreeMap<String, Obj>,
    integration: Option<Integration>,
}

#[derive(Debug, Clone, Default)]
struct Resource {
    info: Obj,
    methods: BTreeMap<String, Method>,
}

impl Resource {
    fn path(&self) -> &str {
        self.info.get("path").and_then(Value::as_str).unwrap_or("/")
    }
}

#[derive(Debug, Clone, Default)]
struct Deployment {
    info: Obj,
    resources: IndexMap<String, Resource>,
}

#[derive(Debug, Clone, Default)]
struct RestApi {
    info: Obj,
    resources: IndexMap<String, Resource>,
    deployments: IndexMap<String, Deployment>,
    stages: BTreeMap<String, Obj>,
    authorizers: IndexMap<String, Obj>,
    models: IndexMap<String, Obj>,
    validators: IndexMap<String, Obj>,
    gateway_responses: BTreeMap<String, Obj>,
    doc_parts: IndexMap<String, Obj>,
    doc_versions: IndexMap<String, Obj>,
}

#[derive(Debug, Default)]
struct UsagePlan {
    info: Obj,
    keys: IndexMap<String, Obj>,
}

#[derive(Debug, Default)]
struct Region {
    apis: IndexMap<String, RestApi>,
    api_keys: IndexMap<String, Obj>,
    usage_plans: IndexMap<String, UsagePlan>,
    domains: IndexMap<String, Obj>,
    mappings: BTreeMap<String, IndexMap<String, Obj>>,
    certificates: IndexMap<String, Obj>,
    vpc_links: IndexMap<String, Obj>,
    associations: IndexMap<String, Obj>,
    account: Obj,
}

#[derive(Default)]
pub struct ApiGateway {
    regions: HashMap<(String, String), Region>,
    /// Requests waiting on a call (a Lambda authorizer or integration).
    pending: HashMap<String, invoke::Pending>,
}

impl ApiGateway {
    fn region(&mut self, ctx: &Ctx<'_>) -> &mut Region {
        self.regions
            .entry((ctx.account.clone(), ctx.region.clone()))
            .or_default()
    }
}

fn api<'r>(
    region: &'r mut Region,
    ctx: &Ctx<'_>,
    input: &Value,
) -> Result<&'r mut RestApi, AwsError> {
    let id = input
        .get("restApiId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    region.apis.get_mut(id).ok_or_else(|| {
        store::not_found(format!(
            "Invalid API identifier specified {}:{id}",
            ctx.account
        ))
    })
}

impl Service for ApiGateway {
    fn signing_name(&self) -> &'static str {
        "apigateway"
    }

    fn model(&self) -> &'static ServiceModel {
        embedded_model!("apigateway")
    }

    fn accepts(&self, req: &Request) -> bool {
        !req.path.starts_with("/v2/")
    }

    fn call(&mut self, ctx: &mut Ctx<'_>, operation: &str, input: Value) -> OpResult {
        if operation == "TestInvokeMethod" {
            return invoke::test_invoke(self, ctx, &input);
        }
        let mut region = std::mem::take(self.region(ctx));
        let result = (|| {
            for part in [
                rest::call,
                rest::methods,
                deploy::call,
                deploy::documents,
                account::call,
                account::domains,
            ] {
                if let Some(out) = part(ctx, &mut region, operation, &input)? {
                    return Ok(out);
                }
            }
            Err(store::bad_request(format!(
                "wasmo: {operation} is not implemented"
            )))
        })();
        *self.region(ctx) = region;
        result
    }

    fn serve(&mut self, ctx: &mut Ctx<'_>, api_id: &str, req: &Request) -> Option<Response> {
        invoke::serve(self, ctx, api_id, req)
    }

    fn answer(&mut self, ctx: &mut Ctx<'_>, id: &str, result: OpResult) {
        invoke::answer(self, ctx, id, result);
    }
}

pub mod codec;
pub mod env;
pub mod model;
pub mod query;
pub mod route;
pub mod service;
pub mod services;
pub mod wire;
pub mod xml;

use serde_json::Value;

use codec::rest::{BodyFormat, HttpRequest};
use codec::{AwsError, json, query as query_codec, rest};
use env::Env;
use model::{Operation, Shape};
use route::Route;
use service::{Ctx, Service, ServiceModel};
use wire::{Request, Response};

pub struct Emulator<E: Env> {
    env: E,
    services: Vec<Box<dyn Service>>,
}

impl<E: Env> Emulator<E> {
    pub fn new(env: E) -> Self {
        Self {
            env,
            services: services::all(),
        }
    }

    pub fn handle(&mut self, req: &Request) -> Response {
        let mut rid = [0u8; 16];
        self.env.random_fill(&mut rid);
        let request_id: String = rid.iter().map(|b| format!("{b:02x}")).collect();
        let route = Route::from_request(req);
        let Some(name) = route.service.as_deref() else {
            return Response::text(
                400,
                "wasmo: unable to determine the target service".to_string(),
            );
        };
        let Some(service) = self.services.iter_mut().find(|s| s.signing_name() == name) else {
            return Response::text(400, format!("wasmo: service '{name}' is not emulated"));
        };
        let model = service.model();
        let mut res = match resolve(model, req) {
            Some(op) => {
                let host = req.header("host").unwrap_or("wasmo.local");
                let mut ctx = Ctx {
                    account: route.account.clone(),
                    region: route.region.clone(),
                    endpoint: format!("http://{host}"),
                    security_token: req.header("x-amz-security-token").map(str::to_string),
                    env: &mut self.env,
                };
                let result = parse_input(model, op, req)
                    .and_then(|input| service.call(&mut ctx, &op.name, input));
                match result {
                    Ok(output) => serialize_output(model, op, &output, &request_id),
                    Err(error) => serialize_error(model, Some(op), &error, &request_id),
                }
            }
            None => {
                let code = match model.metadata.protocol.as_str() {
                    "query" | "ec2" => "InvalidAction",
                    _ => "UnknownOperationException",
                };
                serialize_error(
                    model,
                    None,
                    &AwsError::new(400, code, "wasmo: unable to resolve the operation"),
                    &request_id,
                )
            }
        };
        res.headers
            .push(("x-amzn-requestid".to_string(), request_id.clone()));
        if model.metadata.protocol == "rest-xml" {
            res.headers
                .push(("x-amz-request-id".to_string(), request_id));
        }
        res
    }
}

fn resolve<'m>(model: &'m ServiceModel, req: &Request) -> Option<&'m Operation> {
    match model.metadata.protocol.as_str() {
        "query" | "ec2" => {
            let form = query_form(req);
            let action = query::form_value(&form, "Action")?;
            model.operations.get(action)
        }
        "json" => {
            let target = req.header("x-amz-target")?;
            model
                .operations
                .get(target.rsplit_once('.').map_or(target, |(_, op)| op))
        }
        _ => model.resolve_rest(req),
    }
}

fn query_form(req: &Request) -> Vec<(String, String)> {
    let mut form = query::parse_form(req.query.as_bytes());
    form.extend(query::parse_form(&req.body));
    form
}

fn rest_format(model: &ServiceModel) -> BodyFormat {
    if model.metadata.protocol == "rest-xml" {
        BodyFormat::Xml
    } else {
        BodyFormat::Json
    }
}

fn parse_input(model: &ServiceModel, op: &Operation, req: &Request) -> Result<Value, AwsError> {
    let reg = &model.registry;
    let shape = op.input.as_ref().map(|i| reg.shape(&i.shape));
    match model.metadata.protocol.as_str() {
        "query" => Ok(query_codec::parse_input(
            &query_form(req),
            reg,
            shape,
            query_codec::Dialect::Query,
        )),
        "ec2" => Ok(query_codec::parse_input(
            &query_form(req),
            reg,
            shape,
            query_codec::Dialect::Ec2,
        )),
        "json" => json::parse_input(reg, &req.body, shape),
        _ => {
            let http = HttpRequest {
                path: &req.path,
                query: &req.query,
                headers: &req.headers,
                body: &req.body,
            };
            rest::parse_input(reg, op, &http, rest_format(model))
        }
    }
}

fn serialize_output(
    model: &ServiceModel,
    op: &Operation,
    output: &Value,
    request_id: &str,
) -> Response {
    let reg = &model.registry;
    match model.metadata.protocol.as_str() {
        p @ ("query" | "ec2") => {
            let dialect = if p == "ec2" {
                query_codec::Dialect::Ec2
            } else {
                query_codec::Dialect::Query
            };
            let el = query_codec::serialize_output(
                reg,
                op,
                output,
                model.metadata.xml_namespace.as_deref(),
                request_id,
                dialect,
            );
            Response::new(200, "text/xml", el.to_xml().into_bytes())
        }
        "json" => {
            let shape = op.output.as_ref().map(|o| reg.shape(&o.shape));
            Response::new(
                200,
                &json_content_type(model),
                json::serialize_output(reg, output, shape),
            )
        }
        _ => {
            let r = rest::serialize_output(reg, op, output, rest_format(model));
            Response {
                status: r.status,
                headers: r.headers,
                body: r.body,
            }
        }
    }
}

fn json_content_type(model: &ServiceModel) -> String {
    format!(
        "application/x-amz-json-{}",
        model.metadata.json_version.as_deref().unwrap_or("1.0")
    )
}

fn error_shape<'m>(
    model: &'m ServiceModel,
    op: Option<&Operation>,
    code: &str,
) -> Option<(&'m str, &'m Shape)> {
    let reg = &model.registry;
    let matches = |name: &str| {
        reg.get(name).is_some_and(|s| {
            (s.exception || s.error.is_some())
                && (name == code || s.error.as_ref().and_then(|e| e.code.as_deref()) == Some(code))
        })
    };
    let listed = op
        .and_then(|op| model.operations.get(&op.name))
        .into_iter()
        .flat_map(|op| op.errors.iter().map(|e| e.shape.as_str()))
        .find(|name| matches(name));
    let name = listed.or_else(|| {
        reg.get_key_value(code)
            .filter(|(n, _)| matches(n))
            .map(|(n, _)| n)
    })?;
    Some((name, reg.shape(name)))
}

fn serialize_error(
    model: &ServiceModel,
    op: Option<&Operation>,
    error: &AwsError,
    request_id: &str,
) -> Response {
    let reg = &model.registry;
    let modeled = error_shape(model, op, &error.code);
    let shape = modeled.map(|(_, s)| s);
    let query_code = modeled.map_or_else(
        || error.code.clone(),
        |(name, s)| {
            s.error
                .as_ref()
                .and_then(|e| e.code.clone())
                .unwrap_or_else(|| name.to_string())
        },
    );
    let status = modeled
        .and_then(|(_, s)| s.error.as_ref()?.http_status_code)
        .unwrap_or(error.status);
    let mut error = error.clone();
    error.status = status;
    match model.metadata.protocol.as_str() {
        p @ ("query" | "ec2") => {
            let dialect = if p == "ec2" {
                query_codec::Dialect::Ec2
            } else {
                query_codec::Dialect::Query
            };
            error.code = query_code;
            let el = query_codec::serialize_error(reg, &error, shape, request_id, dialect);
            Response::new(status, "text/xml", el.to_xml().into_bytes())
        }
        "json" => {
            if let Some((name, _)) = modeled {
                error.code = name.to_string();
            }
            let body =
                serde_json::to_vec(&json::error_body(reg, &error, shape)).expect("json serializes");
            let mut res = Response::new(status, &json_content_type(model), body);
            if model.is_query_compatible() {
                let fault = if error.sender_fault {
                    "Sender"
                } else {
                    "Receiver"
                };
                res.headers.push((
                    "x-amzn-query-error".to_string(),
                    format!("{query_code};{fault}"),
                ));
            }
            res
        }
        _ => {
            if model.metadata.protocol == "rest-xml" {
                error.code = query_code;
            } else if let Some((name, _)) = modeled {
                error.code = name.to_string();
            }
            let r = rest::serialize_error(reg, &error, shape, rest_format(model), request_id);
            Response {
                status: r.status,
                headers: r.headers,
                body: r.body,
            }
        }
    }
}

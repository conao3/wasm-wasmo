pub mod chunked;
pub mod codec;
pub mod env;
pub mod model;
pub mod query;
pub mod route;
pub mod service;
pub mod services;
pub mod wire;
pub mod xml;

use std::collections::VecDeque;
use std::io::Read;

use serde_json::Value;

use codec::rest::{BodyFormat, HttpRequest};
use codec::{AwsError, cbor, json, query as query_codec, rest};
use env::Env;
use model::{Operation, Shape};
use route::Route;
use service::{Arn, Ctx, Delivery, Service, ServiceModel};
use wire::{Request, Response};

const MAX_DELIVERY_HOPS: usize = 256;

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
        let target_prefix = req
            .header("x-amz-target")
            .and_then(|t| t.split_once('.'))
            .map(|(p, _)| p);
        let Some(index) = self
            .services
            .iter()
            .position(|s| {
                s.signing_name() == name
                    && target_prefix
                        .is_none_or(|p| s.model().metadata.target_prefix.as_deref() == Some(p))
            })
            .or_else(|| self.services.iter().position(|s| s.signing_name() == name))
        else {
            return Response::text(400, format!("wasmo: service '{name}' is not emulated"));
        };
        let service = &mut self.services[index];
        let model = service.model();
        let mut host = req.header("host").unwrap_or("wasmo.local").to_string();
        let vhost = service
            .virtual_hosted()
            .then(|| route::virtual_host_bucket(&host))
            .flatten()
            .map(|(bucket, base)| (bucket.to_string(), base.to_string()));
        let gzipped = !service.virtual_hosted()
            && req
                .header("content-encoding")
                .is_some_and(|e| e.split(',').any(|x| x.trim() == "gzip"));
        let mut normalized = None;
        if vhost.is_some() || chunked::is_chunked(req) || gzipped {
            let mut r = req.clone();
            if let Some((bucket, base)) = vhost {
                r.path = if r.path == "/" {
                    format!("/{bucket}")
                } else {
                    format!("/{bucket}{}", r.path)
                };
                host = base;
            }
            normalized = Some(r);
        }
        let mut chunk_error = None;
        if let Some(r) = normalized.as_mut().filter(|r| chunked::is_chunked(r))
            && let Err(e) = chunked::unwrap(r)
        {
            chunk_error = Some(AwsError::new(400, "IncompleteBody", e));
        }
        if let Some(r) = normalized.as_mut().filter(|_| gzipped) {
            let mut plain = Vec::new();
            match flate2::read::MultiGzDecoder::new(r.body.as_slice()).read_to_end(&mut plain) {
                Ok(_) => r.body = plain,
                Err(e) => {
                    chunk_error = Some(AwsError::new(
                        400,
                        "SerializationException",
                        format!("gzip body: {e}"),
                    ))
                }
            }
        }
        let req = normalized.as_ref().unwrap_or(req);
        let protocol = request_protocol(model, req);
        let op = service
            .resolve(req)
            .or_else(|| resolve(model, protocol, req));
        let mut outbox = Vec::new();
        let mut res = match (op, chunk_error) {
            (Some(op), Some(error)) => {
                serialize_error(model, protocol, Some(op), &error, &request_id)
            }
            (Some(op), None) => {
                let mut ctx = Ctx::new(
                    route.account.clone(),
                    route.region.clone(),
                    format!("http://{host}"),
                    &mut self.env,
                );
                ctx.security_token = req.header("x-amz-security-token").map(str::to_string);
                let result = parse_input(model, protocol, op, req)
                    .and_then(|input| service.call(&mut ctx, &op.name, input));
                match result {
                    Ok(output) => {
                        outbox = std::mem::take(&mut ctx.outbox);
                        let mut res = serialize_output(model, protocol, op, &output, &request_id);
                        service.finish(&op.name, &output, &mut res);
                        res
                    }
                    Err(error) => serialize_error(model, protocol, Some(op), &error, &request_id),
                }
            }
            (None, _) => {
                let code = match protocol {
                    "query" | "ec2" => "InvalidAction",
                    _ => "UnknownOperationException",
                };
                serialize_error(
                    model,
                    protocol,
                    None,
                    &AwsError::new(400, code, "wasmo: unable to resolve the operation"),
                    &request_id,
                )
            }
        };
        self.route(outbox, &format!("http://{host}"));
        if req.method.eq_ignore_ascii_case("HEAD") || res.status == 304 {
            res.body.clear();
        }
        res.headers
            .push(("x-amzn-requestid".to_string(), request_id.clone()));
        if protocol == "rest-xml" {
            res.headers
                .push(("x-amz-request-id".to_string(), request_id));
        }
        res
    }

    /// Routes deliveries to the services owning the target ARNs, following the
    /// deliveries those services emit in turn.
    fn route(&mut self, outbox: Vec<Delivery>, endpoint: &str) {
        let mut queue: VecDeque<Delivery> = outbox.into();
        let mut hops = 0;
        while let Some(d) = queue.pop_front() {
            hops += 1;
            if hops > MAX_DELIVERY_HOPS {
                break;
            }
            let Some(arn) = Arn::parse(&d.target) else {
                continue;
            };
            let mut ctx = Ctx::new(arn.account, arn.region, endpoint, &mut self.env);
            let delivered = self
                .services
                .iter_mut()
                .find(|s| s.arn_service() == arn.service)
                .is_some_and(|s| s.deliver(&mut ctx, arn.resource, &d));
            queue.extend(ctx.outbox);
            if !delivered && let Some(dlq) = d.dead_letter.clone() {
                queue.push_back(Delivery {
                    target: dlq,
                    dead_letter: None,
                    ..d
                });
            }
        }
    }
}

const CBOR: &str = "smithy-rpc-v2-cbor";

/// Picks the protocol of `req` among those the model lists, so a service
/// offering several (CloudWatch) answers each client in the one it speaks.
fn request_protocol<'m>(model: &'m ServiceModel, req: &Request) -> &'m str {
    let meta = &model.metadata;
    if meta.protocols.len() > 1 {
        let wanted = if req.header("smithy-protocol") == Some("rpc-v2-cbor") {
            CBOR
        } else if req.header("x-amz-target").is_some() {
            "json"
        } else {
            "query"
        };
        if let Some(p) = meta.protocols.iter().find(|p| *p == wanted) {
            return p;
        }
    }
    &meta.protocol
}

fn resolve<'m>(model: &'m ServiceModel, protocol: &str, req: &Request) -> Option<&'m Operation> {
    match protocol {
        CBOR => {
            let (_, op) = req.path.rsplit_once("/operation/")?;
            model.operations.get(op)
        }
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

fn rest_format(protocol: &str) -> BodyFormat {
    if protocol == "rest-xml" {
        BodyFormat::Xml
    } else {
        BodyFormat::Json
    }
}

fn parse_input(
    model: &ServiceModel,
    protocol: &str,
    op: &Operation,
    req: &Request,
) -> Result<Value, AwsError> {
    let reg = &model.registry;
    let shape = op.input.as_ref().map(|i| reg.shape(&i.shape));
    match protocol {
        CBOR => cbor::parse_input(reg, &req.body, shape),
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
            rest::parse_input(reg, op, &http, rest_format(protocol))
        }
    }
}

fn serialize_output(
    model: &ServiceModel,
    protocol: &str,
    op: &Operation,
    output: &Value,
    request_id: &str,
) -> Response {
    let reg = &model.registry;
    match protocol {
        CBOR => {
            let shape = op.output.as_ref().map(|o| reg.shape(&o.shape));
            cbor_response(200, cbor::serialize(reg, output, shape))
        }
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
            let r = rest::serialize_output(reg, op, output, rest_format(protocol));
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
    protocol: &str,
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
    match protocol {
        CBOR => {
            if let Some((name, _)) = modeled {
                error.code = name.to_string();
            }
            let mut res = cbor_response(status, cbor::error_body(reg, &error, shape));
            query_error_header(model, &mut res, &query_code, error.sender_fault);
            res
        }
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
            query_error_header(model, &mut res, &query_code, error.sender_fault);
            res
        }
        _ => {
            if protocol == "rest-xml" {
                error.code = query_code;
            } else if let Some((name, _)) = modeled {
                error.code = name.to_string();
            }
            let r = if model.metadata.service_id.as_deref() == Some("S3") {
                rest::serialize_unwrapped_error(&error, request_id)
            } else {
                rest::serialize_error(reg, &error, shape, rest_format(protocol), request_id)
            };
            Response {
                status: r.status,
                headers: r.headers,
                body: r.body,
            }
        }
    }
}

fn cbor_response(status: u16, body: Vec<u8>) -> Response {
    let mut res = Response::new(status, "application/cbor", body);
    res.headers
        .push(("smithy-protocol".to_string(), "rpc-v2-cbor".to_string()));
    res
}

fn query_error_header(model: &ServiceModel, res: &mut Response, query_code: &str, sender: bool) {
    if model.is_query_compatible() {
        let fault = if sender { "Sender" } else { "Receiver" };
        res.headers.push((
            "x-amzn-query-error".to_string(),
            format!("{query_code};{fault}"),
        ));
    }
}

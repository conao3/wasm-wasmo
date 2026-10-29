pub mod chunked;
pub mod codec;
pub mod env;
pub mod jsonpath;
pub mod model;
pub mod query;
pub mod route;
pub mod sdk;
pub mod service;
pub mod services;
pub mod wire;
pub mod xml;

use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::io::Read;

use serde_json::Value;

use codec::rest::{BodyFormat, HttpRequest};
use codec::{AwsError, cbor, json, query as query_codec, rest};
use env::Env;
use model::{Operation, Shape};
use route::Route;
use service::{Arn, Call, Ctx, Delivery, Effects, OpResult, Service, ServiceModel, Settlement};
use services::lambda::{Completion, InvokeResult, Lambda};
use wire::{Request, Response};

const MAX_DELIVERY_HOPS: usize = 256;
const MAX_STEPS: usize = 100_000;

pub struct Emulator<E: Env> {
    env: E,
    services: Vec<Box<dyn Service>>,
    waiters: HashMap<String, Waiter>,
    /// Responses of waiting requests whose token settled, not yet returned.
    ready: Vec<(String, Response)>,
    deliveries: VecDeque<Delivery>,
    calls: VecDeque<(usize, Call)>,
    settled: VecDeque<(String, Settlement)>,
    /// Count of handled requests; stream event sources that failed retry on a later one.
    tick: u64,
}

/// What waits for a token left in `Ctx::awaiting`.
enum Waiter {
    /// An SDK request whose response is the operation output.
    Sdk(Deferred),
    /// A raw `execute-api` request.
    Raw,
    /// A `Ctx::call` whose caller gets the output through `Service::answer`.
    Call {
        caller: usize,
        id: String,
        account: String,
        region: String,
    },
}

/// A response held back until the token it waits for settles.
struct Deferred {
    service: usize,
    operation: String,
    protocol: &'static str,
    request_id: String,
}

impl<E: Env> Emulator<E> {
    pub fn new(env: E) -> Self {
        Self {
            env,
            services: services::all(),
            waiters: HashMap::new(),
            ready: Vec::new(),
            deliveries: VecDeque::new(),
            calls: VecDeque::new(),
            settled: VecDeque::new(),
            tick: 0,
        }
    }

    pub fn handle(&mut self, req: &Request) -> Response {
        self.tick += 1;
        let mut rid = [0u8; 16];
        self.env.random_fill(&mut rid);
        let request_id: String = rid.iter().map(|b| format!("{b:02x}")).collect();
        let host = req.header("host").unwrap_or("wasmo.local").to_string();
        if let Some(api) = route::execute_api(&host, &req.path) {
            return self.serve(req, api);
        }
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
                    && s.accepts(req)
                    && target_prefix
                        .is_none_or(|p| s.model().metadata.target_prefix.as_deref() == Some(p))
            })
            .or_else(|| self.services.iter().position(|s| s.signing_name() == name))
        else {
            return Response::text(400, format!("wasmo: service '{name}' is not emulated"));
        };
        let service = &mut self.services[index];
        let model = service.model();
        let mut host = host;
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
        let mut effects = Effects::default();
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
                let fx = ctx.effects();
                match result {
                    Ok(output) => {
                        effects = fx;
                        match effects.awaiting.take() {
                            Some(token) => {
                                self.waiters.insert(
                                    token.clone(),
                                    Waiter::Sdk(Deferred {
                                        service: index,
                                        operation: op.name.clone(),
                                        protocol,
                                        request_id: request_id.clone(),
                                    }),
                                );
                                Response::awaiting(token)
                            }
                            None => {
                                let mut res =
                                    serialize_output(model, protocol, op, &output, &request_id);
                                service.finish(&op.name, &output, &mut res);
                                res
                            }
                        }
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
        let endpoint = format!("http://{host}");
        self.absorb(index, effects);
        self.settle_all(&endpoint);
        if let Some(token) = res.awaiting.clone() {
            return self.take_ready(&token).unwrap_or(res);
        }
        if req.method.eq_ignore_ascii_case("HEAD") || res.status == 304 {
            res.body.clear();
        }
        stamp(&mut res, protocol, request_id);
        res
    }

    /// Answers a request to an `execute-api` endpoint through the service
    /// owning the API.
    fn serve(&mut self, req: &Request, api: route::ExecuteApi) -> Response {
        let endpoint = format!("http://{}", req.header("host").unwrap_or("wasmo.local"));
        let mut r = req.clone();
        r.path = api.path;
        for index in 0..self.services.len() {
            let mut ctx = Ctx::new(
                route::DEFAULT_ACCOUNT,
                api.region.clone(),
                endpoint.clone(),
                &mut self.env,
            );
            let Some(res) = self.services[index].serve(&mut ctx, &api.id, &r) else {
                continue;
            };
            let mut fx = ctx.effects();
            let token = fx.awaiting.take();
            if let Some(t) = &token {
                self.waiters.insert(t.clone(), Waiter::Raw);
            }
            self.absorb(index, fx);
            self.settle_all(&endpoint);
            return match token {
                Some(t) => self.take_ready(&t).unwrap_or(Response::awaiting(t)),
                None => res,
            };
        }
        let mut res = Response::new(
            403,
            "application/json",
            br#"{"message":"Forbidden"}"#.to_vec(),
        );
        res.headers.push((
            "x-amzn-errortype".to_string(),
            "ForbiddenException".to_string(),
        ));
        res
    }

    /// Applies the host's answer to an invocation handed out through
    /// `Env::invoke`. Returns the response of a request that waited for it,
    /// directly or through the calls it made.
    pub fn complete(&mut self, meta: &[u8], payload: Vec<u8>) -> Option<Response> {
        let result = match InvokeResult::parse(meta, payload) {
            Ok(r) => r,
            Err(e) => {
                return Some(Response::text(
                    400,
                    format!("wasmo: invalid completion: {e}"),
                ));
            }
        };
        let id = result.id.clone();
        let li = self.lambda_index()?;
        let (account, region, endpoint) = lambda_mut(&mut self.services[li]).scope(&id)?;
        let mut ctx = Ctx::new(account, region, endpoint.clone(), &mut self.env);
        let Completion {
            response,
            sqs_deletes,
        } = lambda_mut(&mut self.services[li]).complete(&mut ctx, result);
        let fx = ctx.effects();
        for delete in sqs_deletes {
            self.delete_messages(&delete.queue_arn, &delete.receipt_handles, &endpoint);
        }
        self.absorb(li, fx);
        if let Some(r) = response {
            self.settled.push_back((id, Settlement::Output(r)));
        }
        self.settle_all(&endpoint);
        (!self.ready.is_empty()).then(|| self.ready.remove(0).1)
    }

    fn take_ready(&mut self, token: &str) -> Option<Response> {
        let at = self.ready.iter().position(|(t, _)| t == token)?;
        Some(self.ready.remove(at).1)
    }

    fn lambda_index(&self) -> Option<usize> {
        self.services
            .iter()
            .position(|s| s.signing_name() == "lambda")
    }

    fn service_index(&self, model: &str) -> Option<usize> {
        self.services.iter().position(|s| s.model().name == model)
    }

    /// Queues what a call of the service at `origin` left behind.
    fn absorb(&mut self, origin: usize, fx: Effects) {
        self.deliveries.extend(fx.outbox);
        self.calls.extend(fx.calls.into_iter().map(|c| (origin, c)));
        self.settled.extend(fx.settled);
    }

    /// Runs deliveries, calls, and settlements until none is left, then polls
    /// the event source mappings, which may start more.
    fn settle_all(&mut self, endpoint: &str) {
        self.drain(endpoint);
        self.pump(endpoint);
        self.drain(endpoint);
    }

    fn drain(&mut self, endpoint: &str) {
        let mut hops = 0;
        for _ in 0..MAX_STEPS {
            if let Some(d) = self.deliveries.pop_front() {
                hops += 1;
                if hops <= MAX_DELIVERY_HOPS {
                    self.deliver(d, endpoint);
                }
            } else if let Some((origin, call)) = self.calls.pop_front() {
                self.run_call(origin, call, endpoint);
            } else if let Some((token, s)) = self.settled.pop_front() {
                self.settle(token, s, endpoint);
            } else {
                return;
            }
        }
        self.deliveries.clear();
        self.calls.clear();
        self.settled.clear();
    }

    fn run_call(&mut self, origin: usize, call: Call, endpoint: &str) {
        let Call {
            id,
            service,
            operation,
            input,
            account,
            region,
            sdk,
        } = call;
        let target = self.service_index(&service);
        let op = target.and_then(|t| self.services[t].model().operations.get(&operation));
        let (Some(target), Some(op)) = (target, op) else {
            let error = AwsError::new(
                400,
                "UnknownOperationException",
                format!("wasmo: {service} {operation} is not emulated"),
            );
            self.answer(origin, &id, &account, &region, Err(error), endpoint);
            return;
        };
        let reg = &self.services[target].model().registry;
        let input = if sdk {
            sdk::input(reg, op, input)
        } else {
            input
        };
        let mut ctx = Ctx::new(account.clone(), region.clone(), endpoint, &mut self.env);
        let result = self.services[target]
            .call(&mut ctx, &operation, input)
            .map(|out| if sdk { sdk::output(reg, op, out) } else { out });
        let mut fx = ctx.effects();
        match result {
            Ok(output) => {
                let token = fx.awaiting.take();
                self.absorb(target, fx);
                match token {
                    Some(token) => {
                        self.waiters.insert(
                            token,
                            Waiter::Call {
                                caller: origin,
                                id,
                                account,
                                region,
                            },
                        );
                    }
                    None => self.answer(origin, &id, &account, &region, Ok(output), endpoint),
                }
            }
            Err(error) => self.answer(origin, &id, &account, &region, Err(error), endpoint),
        }
    }

    fn answer(
        &mut self,
        caller: usize,
        id: &str,
        account: &str,
        region: &str,
        result: OpResult,
        endpoint: &str,
    ) {
        let mut ctx = Ctx::new(account, region, endpoint, &mut self.env);
        self.services[caller].answer(&mut ctx, id, result);
        let fx = ctx.effects();
        self.absorb(caller, fx);
    }

    fn settle(&mut self, token: String, settlement: Settlement, endpoint: &str) {
        let Some(waiter) = self.waiters.remove(&token) else {
            return;
        };
        match (waiter, settlement) {
            (Waiter::Sdk(d), Settlement::Output(result)) => {
                let service = &self.services[d.service];
                let model = service.model();
                let Some(op) = model.operations.get(&d.operation) else {
                    return;
                };
                let mut res = match result {
                    Ok(output) => {
                        let mut res =
                            serialize_output(model, d.protocol, op, &output, &d.request_id);
                        service.finish(&op.name, &output, &mut res);
                        res
                    }
                    Err(error) => {
                        serialize_error(model, d.protocol, Some(op), &error, &d.request_id)
                    }
                };
                stamp(&mut res, d.protocol, d.request_id);
                self.ready.push((token, res));
            }
            (Waiter::Raw, Settlement::Http(res)) | (Waiter::Sdk(_), Settlement::Http(res)) => {
                self.ready.push((token, res));
            }
            (Waiter::Raw, Settlement::Output(_)) => {
                self.ready.push((
                    token,
                    Response::new(
                        502,
                        "application/json",
                        br#"{"message": "Internal server error"}"#.to_vec(),
                    ),
                ));
            }
            (
                Waiter::Call {
                    caller,
                    id,
                    account,
                    region,
                },
                settlement,
            ) => {
                let result = match settlement {
                    Settlement::Output(r) => r,
                    Settlement::Http(res) => Ok(serde_json::json!({
                        "status": res.status,
                        "body": String::from_utf8_lossy(&res.body),
                    })),
                };
                self.answer(caller, &id, &account, &region, result, endpoint);
            }
        }
    }

    /// Polls the enabled event source mappings and hands each new batch to
    /// its function.
    fn pump(&mut self, endpoint: &str) {
        let Some(li) = self.lambda_index() else {
            return;
        };
        let polls = lambda_mut(&mut self.services[li]).polls(self.tick);
        for poll in polls {
            let Some(si) = self.service_index(poll.source_model()) else {
                continue;
            };
            let mut ctx = Ctx::new(
                poll.account.clone(),
                poll.region.clone(),
                endpoint,
                &mut self.env,
            );
            let batch = poll.fetch(self.services[si].as_mut(), &mut ctx);
            let deletes = lambda_mut(&mut self.services[li]).dispatch(&mut ctx, poll, batch);
            let fx = ctx.effects();
            for delete in deletes {
                self.delete_messages(&delete.queue_arn, &delete.receipt_handles, endpoint);
            }
            self.absorb(li, fx);
        }
    }

    fn delete_messages(&mut self, queue_arn: &str, receipt_handles: &[String], endpoint: &str) {
        let (Some(arn), Some(si)) = (Arn::parse(queue_arn), self.service_index("sqs")) else {
            return;
        };
        let mut ctx = Ctx::new(arn.account, arn.region, endpoint, &mut self.env);
        for chunk in receipt_handles.chunks(10) {
            let entries: Vec<Value> = chunk
                .iter()
                .enumerate()
                .map(|(i, h)| serde_json::json!({"Id": i.to_string(), "ReceiptHandle": h}))
                .collect();
            let input = serde_json::json!({
                "QueueUrl": format!("{endpoint}/{}/{}", arn.account, arn.resource),
                "Entries": entries,
            });
            let _ = self.services[si].call(&mut ctx, "DeleteMessageBatch", input);
        }
    }

    /// Hands a delivery to the service owning the target ARN, or to its dead
    /// letter target when the service does not accept it.
    fn deliver(&mut self, d: Delivery, endpoint: &str) {
        let Some(arn) = Arn::parse(&d.target) else {
            return;
        };
        let mut ctx = Ctx::new(arn.account, arn.region, endpoint, &mut self.env);
        let index = self
            .services
            .iter()
            .position(|s| s.arn_service() == arn.service);
        let delivered = index.is_some_and(|i| self.services[i].deliver(&mut ctx, arn.resource, &d));
        let fx = ctx.effects();
        if let Some(i) = index {
            self.absorb(i, fx);
        }
        if !delivered && let Some(dlq) = d.dead_letter.clone() {
            self.deliveries.push_back(Delivery {
                target: dlq,
                dead_letter: None,
                ..d
            });
        }
    }
}

const CBOR: &str = "smithy-rpc-v2-cbor";
/// awsJson with a CBOR body (`application/x-amz-cbor-1.1`), which the AWS SDK
/// for Java sends to Kinesis.
const AWS_CBOR: &str = "aws-cbor";
const AWS_CBOR_CONTENT_TYPE: &str = "application/x-amz-cbor-1.1";

/// Picks the protocol of `req` among those the model lists, so a service
/// offering several (CloudWatch) answers each client in the one it speaks.
fn request_protocol<'m>(model: &'m ServiceModel, req: &Request) -> &'m str {
    let meta = &model.metadata;
    if meta.protocol == "json"
        && req
            .header("content-type")
            .is_some_and(|c| c.starts_with("application/x-amz-cbor"))
    {
        return AWS_CBOR;
    }
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
        "json" | AWS_CBOR => {
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
        CBOR => cbor::parse_input(reg, &req.body, shape, cbor::Epoch::Seconds),
        AWS_CBOR => cbor::parse_input(reg, &req.body, shape, cbor::Epoch::Millis),
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
            cbor_response(
                200,
                cbor::serialize(reg, output, shape, cbor::Epoch::Seconds),
            )
        }
        AWS_CBOR => {
            let shape = op.output.as_ref().map(|o| reg.shape(&o.shape));
            Response::new(
                200,
                AWS_CBOR_CONTENT_TYPE,
                cbor::serialize(reg, output, shape, cbor::Epoch::Millis),
            )
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
                awaiting: None,
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
            let mut res = cbor_response(
                status,
                cbor::error_body(reg, &error, shape, cbor::Epoch::Seconds),
            );
            query_error_header(model, &mut res, &query_code, error.sender_fault);
            res
        }
        AWS_CBOR => {
            if let Some((name, _)) = modeled {
                error.code = name.to_string();
            }
            let body = cbor::error_body(reg, &error, shape, cbor::Epoch::Millis);
            let mut res = Response::new(status, AWS_CBOR_CONTENT_TYPE, body);
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
                awaiting: None,
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

fn lambda_mut(service: &mut Box<dyn Service>) -> &mut Lambda {
    (service.as_mut() as &mut dyn Any)
        .downcast_mut::<Lambda>()
        .expect("the lambda service is Lambda")
}

fn stamp(res: &mut Response, protocol: &str, request_id: String) {
    res.headers
        .push(("x-amzn-requestid".to_string(), request_id.clone()));
    if protocol == "rest-xml" {
        res.headers
            .push(("x-amz-request-id".to_string(), request_id));
    }
}

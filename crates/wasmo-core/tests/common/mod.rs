#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use wasmo_core::Emulator;
use wasmo_core::env::Env;
use wasmo_core::wire::{Request, RequestMeta, Response};

type Queue = Rc<RefCell<Vec<(Value, Vec<u8>)>>>;
/// A handler answers with the response or the error type and message.
pub type Handler = Box<dyn FnMut(&Value) -> Result<Value, (String, String)>>;

struct Host {
    seed: u64,
    queue: Queue,
}

impl Env for Host {
    fn now_ms(&self) -> f64 {
        1_700_000_000_000.0
    }

    fn random_fill(&mut self, buf: &mut [u8]) {
        for b in buf {
            self.seed ^= self.seed << 13;
            self.seed ^= self.seed >> 7;
            self.seed ^= self.seed << 17;
            *b = self.seed as u8;
        }
    }

    fn invoke(&mut self, meta: &[u8], payload: &[u8]) {
        self.queue
            .borrow_mut()
            .push((serde_json::from_slice(meta).unwrap(), payload.to_vec()));
    }
}

/// An emulator whose Lambda invocations run Rust closures, the way an
/// adapter runs host handlers between `wasmo_handle` and `wasmo_complete`.
pub struct Mock {
    emu: Emulator<Host>,
    queue: Queue,
    handlers: HashMap<String, Handler>,
    pub invocations: Vec<(String, Value)>,
}

impl Mock {
    pub fn new() -> Self {
        let queue = Queue::default();
        Self {
            emu: Emulator::new(Host {
                seed: 11,
                queue: queue.clone(),
            }),
            queue,
            handlers: HashMap::new(),
            invocations: Vec::new(),
        }
    }

    pub fn register(
        &mut self,
        name: &str,
        f: impl FnMut(&Value) -> Result<Value, (String, String)> + 'static,
    ) {
        self.handlers.insert(name.to_string(), Box::new(f));
    }

    fn drive(&mut self, mut res: Response) -> Response {
        loop {
            let batch: Vec<_> = self.queue.borrow_mut().drain(..).collect();
            if batch.is_empty() {
                break;
            }
            for (meta, payload) in batch {
                let name = meta["functionName"].as_str().unwrap().to_string();
                let event: Value = serde_json::from_slice(&payload).unwrap_or(Value::Null);
                self.invocations.push((name.clone(), event.clone()));
                let (result, body) = match self.handlers.get_mut(&name) {
                    None => (json!({ "id": meta["id"], "registered": false }), Vec::new()),
                    Some(h) => match h(&event) {
                        Ok(v) => (json!({ "id": meta["id"] }), serde_json::to_vec(&v).unwrap()),
                        Err((kind, message)) => (
                            json!({ "id": meta["id"], "error": { "type": kind, "message": message } }),
                            Vec::new(),
                        ),
                    },
                };
                if let Some(r) = self.emu.complete(result.to_string().as_bytes(), body) {
                    res = r;
                }
            }
        }
        assert!(
            res.awaiting.is_none(),
            "response still awaits {:?}",
            res.awaiting
        );
        res
    }

    pub fn raw(
        &mut self,
        method: &str,
        url: &str,
        headers: Vec<(&str, &str)>,
        body: &[u8],
    ) -> Response {
        let host = url
            .split("://")
            .nth(1)
            .unwrap_or(url)
            .split('/')
            .next()
            .unwrap()
            .to_string();
        let mut h = vec![("host".to_string(), host)];
        h.extend(
            headers
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string())),
        );
        let req = Request::from_meta(
            RequestMeta {
                method: method.into(),
                url: url.into(),
                headers: h,
            },
            body.to_vec(),
        );
        let res = self.emu.handle(&req);
        self.drive(res)
    }

    pub fn request(
        &mut self,
        service: &str,
        method: &str,
        path: &str,
        headers: Vec<(&str, String)>,
        body: Vec<u8>,
    ) -> Response {
        let mut h = vec![(
            "authorization".to_string(),
            format!(
                "AWS4-HMAC-SHA256 Credential=test/20260101/us-east-1/{service}/aws4_request, SignedHeaders=host, Signature=x"
            ),
        )];
        h.extend(headers.into_iter().map(|(k, v)| (k.to_string(), v)));
        let req = Request::from_meta(
            RequestMeta {
                method: method.into(),
                url: format!("http://wasmo.local{path}"),
                headers: h,
            },
            body,
        );
        let res = self.emu.handle(&req);
        self.drive(res)
    }

    /// An awsJson call; returns the status and the decoded body.
    pub fn json(&mut self, service: &str, target: &str, body: Value) -> (u16, Value) {
        let version = if target.starts_with("AmazonSQS")
            || target.starts_with("AWSStepFunctions")
            || target.starts_with("DynamoDB")
        {
            "1.0"
        } else {
            "1.1"
        };
        let res = self.request(
            service,
            "POST",
            "/",
            vec![
                ("content-type", format!("application/x-amz-json-{version}")),
                ("x-amz-target", target.to_string()),
            ],
            body.to_string().into_bytes(),
        );
        (
            res.status,
            serde_json::from_slice(&res.body).unwrap_or(Value::Null),
        )
    }

    pub fn ok(&mut self, service: &str, target: &str, body: Value) -> Value {
        let (status, out) = self.json(service, target, body);
        assert_eq!(status, 200, "{target}: {out}");
        out
    }

    pub fn sfn(&mut self, op: &str, body: Value) -> Value {
        self.ok("states", &format!("AWSStepFunctions.{op}"), body)
    }

    pub fn rest(&mut self, service: &str, method: &str, path: &str, body: Value) -> (u16, Value) {
        let bytes = if body.is_null() {
            Vec::new()
        } else {
            body.to_string().into_bytes()
        };
        let res = self.request(
            service,
            method,
            path,
            vec![("content-type", "application/json".into())],
            bytes,
        );
        (
            res.status,
            serde_json::from_slice(&res.body).unwrap_or(Value::Null),
        )
    }

    pub fn function(&mut self, name: &str) -> String {
        let (status, out) = self.rest(
            "lambda",
            "POST",
            "/2015-03-31/functions",
            json!({
                "FunctionName": name,
                "Runtime": "nodejs20.x",
                "Role": "arn:aws:iam::000000000000:role/r",
                "Handler": "index.handler",
                "Code": { "ZipFile": STANDARD.encode(b"PK fake zip") },
            }),
        );
        assert_eq!(status, 201, "{out}");
        out["FunctionArn"].as_str().unwrap().to_string()
    }

    pub fn queue(&mut self, name: &str) -> (String, String) {
        let url = self.ok("sqs", "AmazonSQS.CreateQueue", json!({ "QueueName": name }))["QueueUrl"]
            .as_str()
            .unwrap()
            .to_string();
        let arn = self.ok(
            "sqs",
            "AmazonSQS.GetQueueAttributes",
            json!({ "QueueUrl": url, "AttributeNames": ["QueueArn"] }),
        )["Attributes"]["QueueArn"]
            .as_str()
            .unwrap()
            .to_string();
        (url, arn)
    }

    pub fn receive(&mut self, url: &str) -> Vec<Value> {
        self.ok(
            "sqs",
            "AmazonSQS.ReceiveMessage",
            json!({ "QueueUrl": url, "MaxNumberOfMessages": 10 }),
        )["Messages"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }
}

impl Mock {
    /// An awsQuery call; returns the XML body.
    pub fn query(&mut self, service: &str, params: &[(&str, &str)]) -> String {
        let body: Vec<String> = params
            .iter()
            .map(|(k, v)| format!("{k}={}", encode(v)))
            .collect();
        let res = self.request(
            service,
            "POST",
            "/",
            vec![("content-type", "application/x-www-form-urlencoded".into())],
            body.join("&").into_bytes(),
        );
        let text = String::from_utf8(res.body).unwrap();
        assert_eq!(res.status, 200, "{text}");
        text
    }
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The text of the first `<tag>` element.
pub fn xml_text(xml: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let start = xml.find(&open).unwrap() + open.len();
    let end = xml[start..].find(&format!("</{tag}>")).unwrap() + start;
    xml[start..end].to_string()
}

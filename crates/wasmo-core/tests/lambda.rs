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
type Handler = Box<dyn Fn(&Value, &Value) -> Result<Value, String>>;

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

struct Mock {
    emu: Emulator<Host>,
    queue: Queue,
    handlers: HashMap<String, Handler>,
    calls: Rc<RefCell<Vec<(String, Value)>>>,
}

impl Mock {
    fn new() -> Self {
        let queue = Queue::default();
        Self {
            emu: Emulator::new(Host {
                seed: 7,
                queue: queue.clone(),
            }),
            queue,
            handlers: HashMap::new(),
            calls: Rc::default(),
        }
    }

    fn register(
        &mut self,
        name: &str,
        f: impl Fn(&Value, &Value) -> Result<Value, String> + 'static,
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
                let event: Value = serde_json::from_slice(&payload).unwrap();
                self.calls.borrow_mut().push((name.clone(), event.clone()));
                let (result, body) = match self.handlers.get(&name) {
                    None => (json!({ "id": meta["id"], "registered": false }), Vec::new()),
                    Some(h) => match h(&event, &meta) {
                        Ok(v) => (
                            json!({ "id": meta["id"], "log": "handled\n" }),
                            serde_json::to_vec(&v).unwrap(),
                        ),
                        Err(m) => (
                            json!({ "id": meta["id"], "error": { "type": "Error", "message": m } }),
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
            "response still awaits an invocation"
        );
        res
    }

    fn request(
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

    fn lambda(&mut self, method: &str, path: &str, body: Value) -> (u16, Value) {
        let bytes = if body.is_null() {
            Vec::new()
        } else {
            body.to_string().into_bytes()
        };
        let res = self.request("lambda", method, path, vec![], bytes);
        let v = serde_json::from_slice(&res.body).unwrap_or(Value::Null);
        (res.status, v)
    }

    fn invoke(&mut self, name: &str, payload: &str, headers: Vec<(&str, String)>) -> Response {
        self.request(
            "lambda",
            "POST",
            &format!("/2015-03-31/functions/{name}/invocations"),
            headers,
            payload.as_bytes().to_vec(),
        )
    }

    fn sqs(&mut self, op: &str, body: Value) -> Value {
        let res = self.request(
            "sqs",
            "POST",
            "/",
            vec![
                ("content-type", "application/x-amz-json-1.0".into()),
                ("x-amz-target", format!("AmazonSQS.{op}")),
            ],
            body.to_string().into_bytes(),
        );
        assert_eq!(
            res.status,
            200,
            "{op}: {}",
            String::from_utf8_lossy(&res.body)
        );
        serde_json::from_slice(&res.body).unwrap()
    }

    fn create(&mut self, name: &str) -> Value {
        let (status, out) = self.lambda(
            "POST",
            "/2015-03-31/functions",
            json!({
                "FunctionName": name,
                "Runtime": "nodejs20.x",
                "Role": "arn:aws:iam::000000000000:role/r",
                "Handler": "index.handler",
                "Code": { "ZipFile": STANDARD.encode(b"PK fake zip") },
                "Environment": { "Variables": { "STAGE": "test" } },
            }),
        );
        assert_eq!(status, 201, "{out}");
        out
    }
}

fn header<'a>(res: &'a Response, name: &str) -> Option<&'a str> {
    res.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

#[test]
fn invoke_runs_the_registered_handler() {
    let mut m = Mock::new();
    m.create("echo");
    m.register("echo", |event, meta| {
        Ok(json!({ "got": event, "stage": meta["environment"]["STAGE"] }))
    });
    let res = m.invoke(
        "echo",
        r#"{"a":1}"#,
        vec![("x-amz-log-type", "Tail".into())],
    );
    assert_eq!(res.status, 200);
    assert_eq!(header(&res, "x-amz-executed-version"), Some("$LATEST"));
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body, json!({ "got": { "a": 1 }, "stage": "test" }));
    let log = String::from_utf8(
        STANDARD
            .decode(header(&res, "x-amz-log-result").unwrap())
            .unwrap(),
    )
    .unwrap();
    assert!(log.starts_with("START RequestId: "), "{log}");
    assert!(log.contains("handled\nEND RequestId: "), "{log}");
}

#[test]
fn handler_errors_are_function_errors() {
    let mut m = Mock::new();
    m.create("boom");
    m.register("boom", |_, _| Err("kaboom".into()));
    let res = m.invoke("boom", "{}", vec![]);
    assert_eq!(res.status, 200);
    assert_eq!(header(&res, "x-amz-function-error"), Some("Unhandled"));
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(
        body,
        json!({ "errorType": "Error", "errorMessage": "kaboom" })
    );

    m.create("orphan");
    let res = m.invoke("orphan", "{}", vec![]);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(header(&res, "x-amz-function-error"), Some("Unhandled"));
    assert_eq!(body["errorType"], "Runtime.HandlerNotFound");

    let res = m.invoke("missing", "{}", vec![]);
    assert_eq!(res.status, 404);
    assert_eq!(
        header(&res, "x-amzn-errortype"),
        Some("ResourceNotFoundException")
    );

    let res = m.invoke("boom", "not json", vec![]);
    assert_eq!(res.status, 400);
    assert_eq!(
        header(&res, "x-amzn-errortype"),
        Some("InvalidRequestContentException")
    );
}

#[test]
fn aliases_route_to_versions() {
    let mut m = Mock::new();
    m.create("v");
    m.register("v", |_, meta| Ok(meta["functionVersion"].clone()));
    let (status, v1) = m.lambda("POST", "/2015-03-31/functions/v/versions", json!({}));
    assert_eq!((status, v1["Version"].as_str()), (201, Some("1")));
    let (status, alias) = m.lambda(
        "POST",
        "/2015-03-31/functions/v/aliases",
        json!({ "Name": "live", "FunctionVersion": "1" }),
    );
    assert_eq!(status, 201, "{alias}");
    assert!(
        alias["AliasArn"]
            .as_str()
            .unwrap()
            .ends_with(":function:v:live")
    );
    let res = m.invoke("v:live", "{}", vec![]);
    assert_eq!(header(&res, "x-amz-executed-version"), Some("1"));
    assert_eq!(res.body, br#""1""#);
}

#[test]
fn sqs_event_source_mapping_consumes_messages() {
    let mut m = Mock::new();
    m.create("consumer");
    let seen = Rc::new(RefCell::new(Vec::new()));
    let s = seen.clone();
    m.register("consumer", move |event, _| {
        for r in event["Records"].as_array().unwrap() {
            s.borrow_mut().push(r["body"].as_str().unwrap().to_string());
        }
        let failed: Vec<Value> = event["Records"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["body"] == "bad")
            .map(|r| json!({ "itemIdentifier": r["messageId"] }))
            .collect();
        Ok(json!({ "batchItemFailures": failed }))
    });
    let url = m.sqs("CreateQueue", json!({ "QueueName": "jobs" }))["QueueUrl"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, esm) = m.lambda(
        "POST",
        "/2015-03-31/event-source-mappings",
        json!({
            "FunctionName": "consumer",
            "EventSourceArn": "arn:aws:sqs:us-east-1:000000000000:jobs",
            "BatchSize": 5,
            "FunctionResponseTypes": ["ReportBatchItemFailures"],
        }),
    );
    assert_eq!(status, 202, "{esm}");
    m.sqs(
        "SendMessage",
        json!({ "QueueUrl": url, "MessageBody": "one" }),
    );
    m.sqs(
        "SendMessage",
        json!({ "QueueUrl": url, "MessageBody": "bad" }),
    );
    assert_eq!(*seen.borrow(), vec!["one", "bad"]);
    let attrs = m.sqs(
        "GetQueueAttributes",
        json!({ "QueueUrl": url, "AttributeNames": ["All"] }),
    );
    assert_eq!(attrs["Attributes"]["ApproximateNumberOfMessages"], "0");
    assert_eq!(
        attrs["Attributes"]["ApproximateNumberOfMessagesNotVisible"],
        "1"
    );
}

#[test]
fn async_failures_reach_the_on_failure_destination() {
    let mut m = Mock::new();
    m.create("flaky");
    m.register("flaky", |_, _| Err("nope".into()));
    let url = m.sqs("CreateQueue", json!({ "QueueName": "failures" }))["QueueUrl"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, out) = m.lambda(
        "PUT",
        "/2019-09-25/functions/flaky/event-invoke-config",
        json!({
            "MaximumRetryAttempts": 1,
            "DestinationConfig": { "OnFailure": { "Destination": "arn:aws:sqs:us-east-1:000000000000:failures" } },
        }),
    );
    assert_eq!(status, 200, "{out}");
    let res = m.invoke(
        "flaky",
        r#"{"n":1}"#,
        vec![("x-amz-invocation-type", "Event".into())],
    );
    assert_eq!(res.status, 202);
    assert_eq!(m.calls.borrow().len(), 2);
    let msgs = m.sqs("ReceiveMessage", json!({ "QueueUrl": url }));
    let record: Value =
        serde_json::from_str(msgs["Messages"][0]["Body"].as_str().unwrap()).unwrap();
    assert_eq!(record["requestContext"]["condition"], "RetriesExhausted");
    assert_eq!(record["requestContext"]["approximateInvokeCount"], 2);
    assert_eq!(record["requestPayload"], json!({ "n": 1 }));
    assert_eq!(record["responsePayload"]["errorMessage"], "nope");
}

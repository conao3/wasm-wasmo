use std::fs;
use std::path::PathBuf;

use serde_json::Value;

const MODELS: &[&str] = &[
    "sts",
    "sqs",
    "dynamodb",
    "dynamodbstreams",
    "s3",
    "sns",
    "events",
    "secretsmanager",
    "ssm",
    "kms",
    "kinesis",
    "firehose",
    "logs",
    "cloudwatch",
    "iam",
    "lambda",
    "stepfunctions",
    "apigateway",
    "apigatewayv2",
];
const STRIPPED_KEYS: &[&str] = &[
    "documentation",
    "documentationUrl",
    "examples",
    "deprecatedMessage",
];

fn strip(v: &mut Value) {
    match v {
        Value::Object(o) => {
            for k in STRIPPED_KEYS {
                o.remove(*k);
            }
            o.values_mut().for_each(strip);
        }
        Value::Array(a) => a.iter_mut().for_each(strip),
        _ => {}
    }
}

fn main() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../vendor/aws-models");
    let out =
        PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo")).join("models");
    fs::create_dir_all(&out).expect("create models dir");
    for name in MODELS {
        let path = src.join(format!("{name}.json"));
        println!("cargo::rerun-if-changed={}", path.display());
        let mut model: Value =
            serde_json::from_slice(&fs::read(&path).expect("read model")).expect("model is json");
        strip(&mut model);
        fs::write(
            out.join(format!("{name}.json")),
            serde_json::to_vec(&model).expect("serialize model"),
        )
        .expect("write model");
    }
}

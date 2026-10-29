use super::*;
use crate::env::FixedEnv;

fn call(sm: &mut SecretsManager, env: &mut FixedEnv, op: &str, input: Value) -> OpResult {
    let mut ctx = Ctx::new("000000000000", "us-east-1", "http://wasmo.local", env);
    sm.call(&mut ctx, op, input)
}

fn env() -> FixedEnv {
    FixedEnv {
        now_ms: 1_700_000_000_000.0,
        seed: 7,
    }
}

fn stages(sm: &mut SecretsManager, env: &mut FixedEnv) -> Value {
    call(sm, env, "DescribeSecret", json!({ "SecretId": "s" })).unwrap()["VersionIdsToStages"]
        .clone()
}

#[test]
fn stage_moves_track_current_and_previous() {
    let mut sm = SecretsManager::default();
    let mut env = env();
    let e = &mut env;
    call(
        &mut sm,
        e,
        "CreateSecret",
        json!({ "Name": "s", "SecretString": "1", "ClientRequestToken": "v1" }),
    )
    .unwrap();
    call(
        &mut sm,
        e,
        "PutSecretValue",
        json!({ "SecretId": "s", "SecretString": "2", "ClientRequestToken": "v2" }),
    )
    .unwrap();
    call(
        &mut sm,
        e,
        "PutSecretValue",
        json!({ "SecretId": "s", "SecretString": "3", "ClientRequestToken": "v3" }),
    )
    .unwrap();
    assert_eq!(
        stages(&mut sm, e),
        json!({ "v2": ["AWSPREVIOUS"], "v3": ["AWSCURRENT"] })
    );
    call(&mut sm, e, "PutSecretValue", json!({ "SecretId": "s", "SecretString": "4", "ClientRequestToken": "v4", "VersionStages": ["AWSPENDING", "custom"] })).unwrap();
    let err = call(
        &mut sm,
        e,
        "UpdateSecretVersionStage",
        json!({ "SecretId": "s", "VersionStage": "AWSCURRENT", "MoveToVersionId": "v4" }),
    )
    .unwrap_err();
    assert_eq!(err.code, "InvalidParameterException");
    call(&mut sm, e, "UpdateSecretVersionStage", json!({ "SecretId": "s", "VersionStage": "AWSCURRENT", "MoveToVersionId": "v4", "RemoveFromVersionId": "v3" })).unwrap();
    assert_eq!(
        stages(&mut sm, e),
        json!({ "v3": ["AWSPREVIOUS"], "v4": ["AWSPENDING", "custom", "AWSCURRENT"] })
    );
    call(&mut sm, e, "UpdateSecretVersionStage", json!({ "SecretId": "s", "VersionStage": "custom", "MoveToVersionId": "v1", "RemoveFromVersionId": "v4" })).unwrap();
    call(
        &mut sm,
        e,
        "UpdateSecretVersionStage",
        json!({ "SecretId": "s", "VersionStage": "AWSPENDING", "RemoveFromVersionId": "v4" }),
    )
    .unwrap();
    assert_eq!(
        stages(&mut sm, e),
        json!({ "v1": ["custom"], "v3": ["AWSPREVIOUS"], "v4": ["AWSCURRENT"] })
    );
    let err = call(
        &mut sm,
        e,
        "UpdateSecretVersionStage",
        json!({ "SecretId": "s", "VersionStage": "AWSCURRENT", "RemoveFromVersionId": "v4" }),
    )
    .unwrap_err();
    assert_eq!(err.code, "InvalidParameterException");
}

#[test]
fn filters_use_prefix_and_negation() {
    let mut sm = SecretsManager::default();
    let mut env = env();
    let e = &mut env;
    call(&mut sm, e, "CreateSecret", json!({ "Name": "prod/db", "Description": "Database password", "Tags": [{ "Key": "team", "Value": "core" }] })).unwrap();
    call(&mut sm, e, "CreateSecret", json!({ "Name": "dev/db" })).unwrap();
    let names = |sm: &mut SecretsManager, e: &mut FixedEnv, filters: Value| -> Vec<String> {
        call(sm, e, "ListSecrets", json!({ "Filters": filters })).unwrap()["SecretList"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["Name"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        names(&mut sm, e, json!([{ "Key": "name", "Values": ["prod"] }])),
        ["prod/db"]
    );
    assert_eq!(
        names(&mut sm, e, json!([{ "Key": "name", "Values": ["db"] }])),
        Vec::<String>::new()
    );
    assert_eq!(
        names(&mut sm, e, json!([{ "Key": "name", "Values": ["!prod"] }])),
        ["dev/db"]
    );
    assert_eq!(
        names(
            &mut sm,
            e,
            json!([{ "Key": "description", "Values": ["data"] }])
        ),
        ["prod/db"]
    );
    assert_eq!(
        names(&mut sm, e, json!([{ "Key": "all", "Values": ["DB"] }])),
        ["prod/db", "dev/db"]
    );
    assert_eq!(
        names(
            &mut sm,
            e,
            json!([{ "Key": "tag-value", "Values": ["co"] }, { "Key": "name", "Values": ["prod"] }])
        ),
        ["prod/db"]
    );
}

#[test]
fn scheduled_deletion_purges_after_window() {
    let mut sm = SecretsManager::default();
    let mut env = env();
    call(
        &mut sm,
        &mut env,
        "CreateSecret",
        json!({ "Name": "s", "SecretString": "x" }),
    )
    .unwrap();
    call(
        &mut sm,
        &mut env,
        "DeleteSecret",
        json!({ "SecretId": "s", "RecoveryWindowInDays": 7 }),
    )
    .unwrap();
    assert!(
        call(
            &mut sm,
            &mut env,
            "DescribeSecret",
            json!({ "SecretId": "s" })
        )
        .is_ok()
    );
    env.now_ms += 8.0 * DAY * 1000.0;
    let err = call(
        &mut sm,
        &mut env,
        "DescribeSecret",
        json!({ "SecretId": "s" }),
    )
    .unwrap_err();
    assert_eq!(err.code, "ResourceNotFoundException");
}

#[test]
fn partial_arn_resolves() {
    let mut sm = SecretsManager::default();
    let mut env = env();
    let out = call(
        &mut sm,
        &mut env,
        "CreateSecret",
        json!({ "Name": "app", "SecretString": "x" }),
    )
    .unwrap();
    let arn = out["ARN"].as_str().unwrap();
    let partial = &arn[..arn.len() - 7];
    for id in [arn, partial] {
        let got = call(
            &mut sm,
            &mut env,
            "GetSecretValue",
            json!({ "SecretId": id }),
        )
        .unwrap();
        assert_eq!(got["SecretString"], "x");
    }
}

# Vendored upstream test data — provenance

All data under `vendor/` is fetched from upstream at a pinned ref and committed
verbatim. No wasmo-side modification has been applied.

## botocore-protocol-tests/

|               |                                                                                                                                                                              |
| ------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Upstream repo | https://github.com/boto/botocore                                                                                                                                             |
| License       | Apache-2.0                                                                                                                                                                   |
| Pinned tag    | `1.43.19`                                                                                                                                                                    |
| Commit hash   | `8fdb47d1636e7f7ae0cacdd4584ffd339f88c546`                                                                                                                                   |
| Source path   | `tests/unit/protocols/`                                                                                                                                                      |
| Fetched on    | 2026-06-02 (JST)                                                                                                                                                             |
| Method        | GitHub Contents API (`api.github.com/repos/boto/botocore/contents/...?ref=<tag>`) for the file listing, `raw.githubusercontent.com/boto/botocore/<commit>/...` for the bytes |

### Layout

```
botocore-protocol-tests/
  input/                 # server deserialize fixtures (serialized request -> params)
    ec2.json
    json.json            # awsJson 1.1
    json_1_0.json        # awsJson 1.0
    json_1_0-query-compatible.json
    query.json
    rest-json.json
    rest-xml.json
    smithy-rpc-v2-cbor.json
    smithy-rpc-v2-cbor-query-compatible.json
    smithy-rpc-v2-cbor-non-query-compatible.json
  output/                # server serialize fixtures (result/error -> serialized response)
    (same file set as input/)
  legacy/
    input/               # ec2 / json / query / rest-json / rest-xml (older fixture shape)
    output/
  protocol-tests-ignore-list.json   # botocore's own skip list
  LICENSE.txt            # upstream Apache-2.0 license text
  NOTICE                 # upstream NOTICE
```

### Notes

- `smithy-rpc-v2-cbor*.json` run in the conformance suite like the other
  protocols.
- The upstream `LICENSE.txt` and `NOTICE` are co-located in
  `botocore-protocol-tests/` to satisfy Apache-2.0 4(a)/4(d). The root `NOTICE`
  propagates the Botocore attribution.

## aws-models/

|               |                                                                                                                                                |
| ------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| Upstream repo | https://github.com/boto/botocore                                                                                                               |
| License       | Apache-2.0                                                                                                                                     |
| Pinned tag    | `1.43.19`                                                                                                                                      |
| Commit hash   | `8fdb47d1636e7f7ae0cacdd4584ffd339f88c546`                                                                                                     |
| Source path   | `botocore/data/<service>/<apiVersion>/service-2.json`                                                                                          |
| Fetched on    | 2026-06-02 (JST)                                                                                                                                |
| Method        | GitHub Contents API (`api.github.com/repos/boto/botocore/contents/botocore/data/<service>?ref=<tag>`) to discover the apiVersion directory, then `raw.githubusercontent.com/boto/botocore/<commit>/botocore/data/<service>/<apiVersion>/service-2.json` for the bytes (verbatim) |


### Layout

```
aws-models/
  sts.json            # botocore/data/sts/2011-06-15/service-2.json            (protocol query)
  sqs.json            # botocore/data/sqs/2012-11-05/service-2.json            (protocol json)
  dynamodb.json       # botocore/data/dynamodb/2012-08-10/service-2.json       (protocol json)
  s3.json             # botocore/data/s3/2006-03-01/service-2.json             (protocol rest-xml)
  sns.json            # botocore/data/sns/2010-03-31/service-2.json            (protocol query)
  events.json         # botocore/data/events/2015-10-07/service-2.json         (protocol json)
  secretsmanager.json # botocore/data/secretsmanager/2017-10-17/service-2.json (protocol json)
  ssm.json            # botocore/data/ssm/2014-11-06/service-2.json            (protocol json)
  kms.json            # botocore/data/kms/2014-11-01/service-2.json            (protocol json)
  dynamodbstreams.json # botocore/data/dynamodbstreams/2012-08-10/service-2.json (protocol json, signingName dynamodb)
  kinesis.json        # botocore/data/kinesis/2013-12-02/service-2.json        (protocol json)
  firehose.json       # botocore/data/firehose/2015-08-04/service-2.json       (protocol json)
```

`crates/wasmo-core/build.rs` strips documentation fields at build time; the
files here stay byte-for-byte upstream.

## parity-fixtures/

|               |                                                                                       |
| ------------- | ------------------------------------------------------------------------------------- |
| Upstream repo | https://github.com/conao3/bun-bunsai                                                   |
| License       | Apache-2.0 (`parity-fixtures/LICENSE`, `parity-fixtures/NOTICE`)                      |
| Commit hash   | `df13799e939f9424b5801d8cf3caea7c5d9fbad2` (fixtures last changed in `34bdc1dfe90190edbc568cd1f26c94a35fefc6e2`) |
| Source path   | `test/parity/fixtures/`                                                               |
| Fetched on    | 2026-10-28 (JST)                                                                      |
| Method        | `git archive <commit> test/parity/fixtures` from a local clone, extracted verbatim    |

### Layout

```
parity-fixtures/
  <service>/<case>.json   # 150 fixtures for 41 services
  LICENSE                 # bun-bunsai Apache-2.0 license text
  NOTICE                  # bun-bunsai NOTICE
```

### Notes

- Each fixture is a sequence of AWS SDK for JavaScript v3 commands (`clientPackage`,
  `clientName`, `steps[].command` / `input` / `expected` / `expectedError` /
  `ignorePaths` / `saveAs`). `<ANY>` matches any value, `${name.path}` refers to
  the output saved by an earlier step, and `{"$base64": ...}` is a byte payload.
- bun-bunsai derives the expectations from real AWS: LocalStack v3.8.1 snapshot
  recordings (Apache-2.0, commit `529aba7d8372e9199f42a31a6500071363ad8c18`),
  moto 5.2.2 `@aws_verified` tests (Apache-2.0, commit
  `837595545cc4a4bb8fede2cff84b2b2373443981`), and the AWS API reference. The
  per-fixture origin is listed in bun-bunsai's `test/vendor/PROVENANCE.md`.
- `crates/wasmo-core/tests/parity.rs` runs the fixtures of the services wasmo
  emulates and skips the rest.

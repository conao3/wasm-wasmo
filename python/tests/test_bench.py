import statistics
import time
from importlib.resources import files

import wasmtime

import wasmo

N = 300


def median_us(n, fn):
    samples = []
    for _ in range(n):
        start = time.perf_counter_ns()
        fn()
        samples.append(time.perf_counter_ns() - start)
    return statistics.median(samples) / 1000


def test_latency(capsys):
    aws = wasmo.Mock()
    ddb = aws.client("dynamodb")
    ddb.create_table(
        TableName="bench",
        AttributeDefinitions=[{"AttributeName": "pk", "AttributeType": "S"}],
        KeySchema=[{"AttributeName": "pk", "KeyType": "HASH"}],
        BillingMode="PAY_PER_REQUEST",
    )
    counter = iter(range(10**9))

    def key():
        return f"k{next(counter) % 100}"

    put = median_us(
        N,
        lambda: ddb.put_item(
            TableName="bench", Item={"pk": {"S": key()}, "n": {"N": "1"}, "s": {"S": "value"}}
        ),
    )
    get = median_us(N, lambda: ddb.get_item(TableName="bench", Key={"pk": {"S": key()}}))

    s3 = aws.client("s3")
    s3.create_bucket(Bucket="bench")
    kib = bytes(i % 251 for i in range(1024))
    s3put = median_us(N, lambda: s3.put_object(Bucket="bench", Key=key(), Body=kib))
    s3get = median_us(N, lambda: s3.get_object(Bucket="bench", Key=key())["Body"].read())
    aws.close()

    def create():
        m = wasmo.Mock()
        m.client("dynamodb").list_tables()
        m.close()

    mock = median_us(50, create)

    def instantiate():
        m = wasmo.Mock()
        m.fetch("http://wasmo.local/")
        m.close()

    instance = median_us(50, instantiate)

    binary = files("wasmo").joinpath("wasmo.wasm").read_bytes()
    start = time.perf_counter()
    wasmtime.Module(wasmtime.Engine(), binary)
    compile_ms = (time.perf_counter() - start) * 1000

    with capsys.disabled():
        print(
            f"\nmedian latency (us): dynamodb put {put:.0f}, get {get:.0f}; "
            f"s3 1KiB put {s3put:.0f}, get {s3get:.0f}; "
            f"mock + first ListTables {mock:.0f}, instance only {instance:.0f}; "
            f"module compile {compile_ms:.0f} ms"
        )

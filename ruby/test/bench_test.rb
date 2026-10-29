# frozen_string_literal: true

require_relative "test_helper"
require "aws-sdk-dynamodb"
require "aws-sdk-s3"

class BenchTest < Minitest::Test
  include WasmoTest

  N = 300

  def median(count)
    samples = Array.new(count) do
      start = Process.clock_gettime(Process::CLOCK_MONOTONIC, :float_microsecond)
      yield
      Process.clock_gettime(Process::CLOCK_MONOTONIC, :float_microsecond) - start
    end.sort
    samples[count / 2].round
  end

  def test_latency
    ddb = @aws.client(Aws::DynamoDB::Client)
    ddb.create_table(
      table_name: "bench",
      attribute_definitions: [{ attribute_name: "pk", attribute_type: "S" }],
      key_schema: [{ attribute_name: "pk", key_type: "HASH" }],
      billing_mode: "PAY_PER_REQUEST"
    )
    i = 0
    put = median(N) do
      ddb.put_item(table_name: "bench", item: { "pk" => "k#{(i += 1) % 100}", "n" => i, "s" => "value" })
    end
    get = median(N) { ddb.get_item(table_name: "bench", key: { "pk" => "k#{(i += 1) % 100}" }) }

    s3 = @aws.client(Aws::S3::Client)
    s3.create_bucket(bucket: "bench")
    kib = pattern(1024)
    s3put = median(N) { s3.put_object(bucket: "bench", key: "k#{(i += 1) % 100}", body: kib) }
    s3get = median(N) { s3.get_object(bucket: "bench", key: "k#{(i += 1) % 100}").body.read }

    bare = median(50) { Wasmo::Mock.new.close }
    create = median(50) do
      m = Wasmo::Mock.new
      m.client(Aws::DynamoDB::Client).list_tables
      m.close
    end

    puts format(
      "\nmedian latency (us): DynamoDB PutItem %d, GetItem %d; S3 1 KiB PutObject %d, GetObject %d; " \
      "Mock.new %d; Mock.new + client + first call %d",
      put, get, s3put, s3get, bare, create
    )
    assert_operator put, :positive?
  end
end

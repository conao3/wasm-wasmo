# frozen_string_literal: true

require_relative "test_helper"
require "aws-sdk-kinesis"

class KinesisTest < Minitest::Test
  include WasmoTest

  def test_put_records_and_get_records
    kinesis = @aws.client(Aws::Kinesis::Client)
    kinesis.create_stream(stream_name: "clicks", shard_count: 2)
    kinesis.wait_until(:stream_exists, { stream_name: "clicks" }, delay: 0, max_attempts: 5)
    put = kinesis.put_records(
      stream_name: "clicks",
      records: Array.new(10) { |i| { partition_key: "user-#{i}", data: "click-#{i}" } }
    )
    assert_equal 0, put.failed_record_count
    assert_equal 10, put.records.size

    got = {}
    kinesis.list_shards(stream_name: "clicks").shards.each do |shard|
      iterator = kinesis.get_shard_iterator(
        stream_name: "clicks", shard_id: shard.shard_id, shard_iterator_type: "TRIM_HORIZON"
      ).shard_iterator
      out = kinesis.get_records(shard_iterator: iterator)
      refute_nil out.next_shard_iterator
      out.records.each do |r|
        assert_kind_of Time, r.approximate_arrival_timestamp
        got[r.partition_key] = r.data
      end
    end
    assert_equal(Array.new(10) { |i| ["user-#{i}", "click-#{i}"] }.to_h, got)
  end
end

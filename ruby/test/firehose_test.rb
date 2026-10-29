# frozen_string_literal: true

require_relative "test_helper"
require "aws-sdk-firehose"
require "aws-sdk-s3"

class FirehoseTest < Minitest::Test
  include WasmoTest

  def test_delivers_to_s3
    s3 = @aws.client(Aws::S3::Client, force_path_style: true)
    firehose = @aws.client(Aws::Firehose::Client)
    s3.create_bucket(bucket: "lake")
    firehose.create_delivery_stream(
      delivery_stream_name: "events",
      extended_s3_destination_configuration: {
        bucket_arn: "arn:aws:s3:::lake", role_arn: "arn:aws:iam::000000000000:role/firehose", prefix: "raw/"
      }
    )
    firehose.put_record(delivery_stream_name: "events", record: { data: "one\n" })
    batch = firehose.put_record_batch(delivery_stream_name: "events", records: [{ data: "two\n" }, { data: "three\n" }])
    assert_equal 0, batch.failed_put_count

    keys = s3.list_objects_v2(bucket: "lake", prefix: "raw/").contents.map(&:key)
    assert_equal 2, keys.size
    assert(keys.all? { |k| k.split("/").last.start_with?("events-1-") })
    assert_equal %W[one\n two\nthree\n], keys.map { |k| s3.get_object(bucket: "lake", key: k).body.read }.sort
  end
end

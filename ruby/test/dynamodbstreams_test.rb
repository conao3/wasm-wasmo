# frozen_string_literal: true

require_relative "test_helper"
require "aws-sdk-dynamodb"
require "aws-sdk-dynamodbstreams"

class DynamoDBStreamsTest < Minitest::Test
  include WasmoTest

  def test_stream_records_after_writes
    db = @aws.client(Aws::DynamoDB::Client)
    streams = @aws.client(Aws::DynamoDBStreams::Client)
    arn = db.create_table(
      table_name: "users",
      attribute_definitions: [{ attribute_name: "id", attribute_type: "S" }],
      key_schema: [{ attribute_name: "id", key_type: "HASH" }],
      billing_mode: "PAY_PER_REQUEST",
      stream_specification: { stream_enabled: true, stream_view_type: "NEW_AND_OLD_IMAGES" }
    ).table_description.latest_stream_arn

    db.put_item(table_name: "users", item: { "id" => "u1", "name" => "ada" })
    db.update_item(
      table_name: "users", key: { "id" => "u1" }, update_expression: "SET #n = :n",
      expression_attribute_names: { "#n" => "name" }, expression_attribute_values: { ":n" => "grace" }
    )
    db.delete_item(table_name: "users", key: { "id" => "u1" })

    assert_equal [arn], streams.list_streams(table_name: "users").streams.map(&:stream_arn)
    shard = streams.describe_stream(stream_arn: arn).stream_description.shards[0]
    iterator = streams.get_shard_iterator(
      stream_arn: arn, shard_id: shard.shard_id, shard_iterator_type: "TRIM_HORIZON"
    ).shard_iterator
    records = streams.get_records(shard_iterator: iterator).records
    assert_equal %w[INSERT MODIFY REMOVE], records.map(&:event_name)
    assert_equal "ada", records[0].dynamodb.new_image["name"].s
    assert_equal "ada", records[1].dynamodb.old_image["name"].s
    assert_equal "grace", records[1].dynamodb.new_image["name"].s
    assert_nil records[2].dynamodb.new_image
  end
end

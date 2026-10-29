# frozen_string_literal: true

require_relative "test_helper"
require "aws-sdk-dynamodb"

class DynamoDBTest < Minitest::Test
  include WasmoTest

  def setup
    super
    @ddb = @aws.client(Aws::DynamoDB::Client)
    @ddb.create_table(
      table_name: "users",
      attribute_definitions: [
        { attribute_name: "id", attribute_type: "S" },
        { attribute_name: "team", attribute_type: "S" },
        { attribute_name: "age", attribute_type: "N" }
      ],
      key_schema: [{ attribute_name: "id", key_type: "HASH" }],
      global_secondary_indexes: [
        {
          index_name: "by-team",
          key_schema: [{ attribute_name: "team", key_type: "HASH" }, { attribute_name: "age", key_type: "RANGE" }],
          projection: { projection_type: "ALL" }
        }
      ],
      billing_mode: "PAY_PER_REQUEST"
    )
  end

  def test_put_and_get
    @ddb.put_item(table_name: "users", item: { "id" => "1", "name" => "Ann", "age" => 30, "tags" => Set["a", "b"] })
    item = @ddb.get_item(table_name: "users", key: { "id" => "1" }).item
    assert_equal "Ann", item["name"]
    assert_equal 30, item["age"]
    assert_equal Set["a", "b"], item["tags"]
  end

  def test_conditional_put_fails
    put = lambda do
      @ddb.put_item(table_name: "users", item: { "id" => "1" }, condition_expression: "attribute_not_exists(id)")
    end
    put.call
    assert_raises(Aws::DynamoDB::Errors::ConditionalCheckFailedException) { put.call }
  end

  def test_update_expression
    @ddb.put_item(table_name: "users", item: { "id" => "1", "age" => 1 })
    out = @ddb.update_item(
      table_name: "users",
      key: { "id" => "1" },
      update_expression: "SET age = age + :d",
      expression_attribute_values: { ":d" => 2 },
      return_values: "ALL_NEW"
    )
    assert_equal 3, out.attributes["age"]
  end

  def test_gsi_query
    [["1", "red", 30], ["2", "red", 20], ["3", "blue", 40]].each do |id, team, age|
      @ddb.put_item(table_name: "users", item: { "id" => id, "team" => team, "age" => age })
    end
    out = @ddb.query(
      table_name: "users",
      index_name: "by-team",
      key_condition_expression: "team = :t AND age > :a",
      expression_attribute_values: { ":t" => "red", ":a" => 10 }
    )
    assert_equal(%w[2 1], out.items.map { |i| i["id"] })
  end

  def test_transact_write_cancels
    @ddb.put_item(table_name: "users", item: { "id" => "1" })
    err = assert_raises(Aws::DynamoDB::Errors::TransactionCanceledException) do
      @ddb.transact_write_items(
        transact_items: [
          { put: { table_name: "users", item: { "id" => "2" } } },
          { put: { table_name: "users", item: { "id" => "1" }, condition_expression: "attribute_not_exists(id)" } }
        ]
      )
    end
    assert_equal %w[None ConditionalCheckFailed], err.cancellation_reasons.map(&:code)
    assert_nil @ddb.get_item(table_name: "users", key: { "id" => "2" }).item
  end
end

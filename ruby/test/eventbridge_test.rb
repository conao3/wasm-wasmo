# frozen_string_literal: true

require_relative "test_helper"
require "aws-sdk-eventbridge"
require "aws-sdk-sqs"

class EventBridgeTest < Minitest::Test
  include WasmoTest

  def test_put_events_to_sqs_target
    events = @aws.client(Aws::EventBridge::Client)
    sqs = @aws.client(Aws::SQS::Client)
    url = sqs.create_queue(queue_name: "orders").queue_url
    events.put_rule(
      name: "big-orders",
      event_pattern: JSON.generate({ source: ["shop"], detail: { total: [{ numeric: [">=", 100] }] } })
    )
    events.put_targets(rule: "big-orders", targets: [{ id: "queue", arn: queue_arn(sqs, url) }])

    put = events.put_events(entries: [150, 20].map do |total|
      { source: "shop", detail_type: "Order Placed", detail: JSON.generate({ total: total }) }
    end)
    assert_equal 0, put.failed_entry_count

    msgs = sqs.receive_message(queue_url: url, max_number_of_messages: 10).messages
    assert_equal 1, msgs.size
    event = JSON.parse(msgs[0].body)
    assert_equal put.data.entries[0].event_id, event["id"]
    assert_equal "Order Placed", event["detail-type"]
    assert_equal({ "total" => 150 }, event["detail"])
  end
end

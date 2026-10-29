# frozen_string_literal: true

require_relative "test_helper"
require "aws-sdk-sqs"

class SQSTest < Minitest::Test
  include WasmoTest

  def setup
    super
    @sqs = @aws.client(Aws::SQS::Client)
  end

  def test_send_and_receive_with_attributes
    url = @sqs.create_queue(queue_name: "jobs").queue_url
    @sqs.send_message(
      queue_url: url,
      message_body: "héllo",
      message_attributes: { "kind" => { data_type: "String", string_value: "greeting" } }
    )
    msgs = @sqs.receive_message(queue_url: url, message_attribute_names: ["All"]).messages
    assert_equal ["héllo"], msgs.map(&:body)
    assert_equal "greeting", msgs[0].message_attributes["kind"].string_value
    @sqs.delete_message(queue_url: url, receipt_handle: msgs[0].receipt_handle)
    assert_empty @sqs.receive_message(queue_url: url).messages
  end

  def test_batch
    url = @sqs.create_queue(queue_name: "batch").queue_url
    out = @sqs.send_message_batch(
      queue_url: url,
      entries: Array.new(3) { |i| { id: "m#{i}", message_body: "body #{i}" } }
    )
    assert_equal 3, out.successful.size
    bodies = @sqs.receive_message(queue_url: url, max_number_of_messages: 10).messages.map(&:body)
    assert_equal ["body 0", "body 1", "body 2"], bodies.sort
  end

  def test_fifo
    url = @sqs.create_queue(
      queue_name: "orders.fifo",
      attributes: { "FifoQueue" => "true", "ContentBasedDeduplication" => "true" }
    ).queue_url
    2.times { @sqs.send_message(queue_url: url, message_body: "once", message_group_id: "g") }
    assert_equal ["once"], @sqs.receive_message(queue_url: url, max_number_of_messages: 10).messages.map(&:body)
  end

  def test_missing_queue
    assert_raises(Aws::SQS::Errors::QueueDoesNotExist) do
      @sqs.get_queue_url(queue_name: "nope")
    end
  end
end

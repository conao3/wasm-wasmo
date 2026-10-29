# frozen_string_literal: true

require_relative "test_helper"
require "aws-sdk-sns"
require "aws-sdk-sqs"

class SNSTest < Minitest::Test
  include WasmoTest

  def test_delivers_to_sqs
    sns = @aws.client(Aws::SNS::Client)
    sqs = @aws.client(Aws::SQS::Client)
    url = sqs.create_queue(queue_name: "inbox").queue_url
    topic = sns.create_topic(name: "events").topic_arn
    sns.subscribe(topic_arn: topic, protocol: "sqs", endpoint: queue_arn(sqs, url))
    sns.publish(topic_arn: topic, message: "hello", subject: "greet")
    msgs = sqs.receive_message(queue_url: url).messages
    assert_equal 1, msgs.size
    envelope = JSON.parse(msgs[0].body)
    assert_equal "Notification", envelope["Type"]
    assert_equal "hello", envelope["Message"]
    assert_equal topic, envelope["TopicArn"]
  end

  def test_raw_delivery_with_filter
    sns = @aws.client(Aws::SNS::Client)
    sqs = @aws.client(Aws::SQS::Client)
    url = sqs.create_queue(queue_name: "raw").queue_url
    topic = sns.create_topic(name: "orders").topic_arn
    sns.subscribe(
      topic_arn: topic,
      protocol: "sqs",
      endpoint: queue_arn(sqs, url),
      attributes: { "RawMessageDelivery" => "true", "FilterPolicy" => JSON.generate({ kind: ["paid"] }) }
    )
    %w[paid refunded].each do |kind|
      sns.publish(
        topic_arn: topic,
        message: kind,
        message_attributes: { "kind" => { data_type: "String", string_value: kind } }
      )
    end
    assert_equal ["paid"], sqs.receive_message(queue_url: url, max_number_of_messages: 10).messages.map(&:body)
  end
end

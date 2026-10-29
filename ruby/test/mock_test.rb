# frozen_string_literal: true

require_relative "test_helper"
require "aws-sdk-s3"
require "aws-sdk-sqs"
require "aws-sdk-sts"

class MockTest < Minitest::Test
  def test_mocks_are_isolated
    a = Wasmo::Mock.new
    b = Wasmo::Mock.new
    a.client(Aws::SQS::Client).create_queue(queue_name: "only-a")
    assert_equal 1, a.client(Aws::SQS::Client).list_queues.queue_urls.size
    assert_empty b.client(Aws::SQS::Client).list_queues.queue_urls
  ensure
    a&.close
    b&.close
  end

  def test_reset_drops_state
    aws = Wasmo::Mock.new
    sqs = aws.client(Aws::SQS::Client)
    sqs.create_queue(queue_name: "q")
    aws.reset
    assert_empty sqs.list_queues.queue_urls
  ensure
    aws&.close
  end

  def test_closed_mock_raises
    aws = Wasmo::Mock.new
    sts = aws.client(Aws::STS::Client)
    aws.close
    assert_raises(Wasmo::ClosedError) { sts.get_caller_identity }
    assert_raises(Wasmo::ClosedError) { aws.reset }
  end

  def test_mock_aws_uses_aws_config
    before = Aws.config.dup
    Wasmo.mock_aws do |aws|
      s3 = Aws::S3::Client.new
      s3.create_bucket(bucket: "global")
      Aws::S3::Client.new(force_path_style: true).put_object(bucket: "global", key: "k", body: "v")
      assert_equal "v", aws.client(Aws::S3::Client).get_object(bucket: "global", key: "k").body.read
      assert_equal "000000000000", Aws::STS::Client.new.get_caller_identity.account
    end
    assert_equal before, Aws.config
  end

  def test_threads_on_separate_mocks
    threads = Array.new(8) do |n|
      Thread.new do
        aws = Wasmo::Mock.new
        sqs = aws.client(Aws::SQS::Client)
        url = sqs.create_queue(queue_name: "t#{n}").queue_url
        20.times { |i| sqs.send_message(queue_url: url, message_body: i.to_s) }
        count = sqs.get_queue_attributes(queue_url: url, attribute_names: ["ApproximateNumberOfMessages"])
                   .attributes["ApproximateNumberOfMessages"]
        aws.close
        count
      end
    end
    assert_equal ["20"] * 8, threads.map(&:value)
  end

  def test_threads_on_one_mock
    aws = Wasmo::Mock.new
    sqs = aws.client(Aws::SQS::Client)
    url = sqs.create_queue(queue_name: "shared").queue_url
    Array.new(8) { |n| Thread.new { 25.times { |i| sqs.send_message(queue_url: url, message_body: "#{n}-#{i}") } } }
         .each(&:join)
    count = sqs.get_queue_attributes(queue_url: url, attribute_names: ["ApproximateNumberOfMessages"])
               .attributes["ApproximateNumberOfMessages"]
    assert_equal "200", count
  ensure
    aws&.close
  end
end

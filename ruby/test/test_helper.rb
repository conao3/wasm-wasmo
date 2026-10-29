# frozen_string_literal: true

require "minitest/autorun"
require "wasmo"

module WasmoTest
  def setup
    @aws = Wasmo::Mock.new
  end

  def teardown
    @aws.close
  end

  def queue_arn(sqs, url)
    sqs.get_queue_attributes(queue_url: url, attribute_names: ["QueueArn"]).attributes["QueueArn"]
  end

  def pattern(size)
    Array.new(size) { |i| ((i * 31) + 7) & 0xff }.pack("C*")
  end
end

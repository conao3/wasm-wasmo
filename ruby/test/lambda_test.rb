# frozen_string_literal: true

require_relative "test_helper"
require "aws-sdk-lambda"
require "aws-sdk-sqs"
require "base64"

class LambdaTest < Minitest::Test
  include WasmoTest

  def setup
    super
    @lambda = @aws.client(Aws::Lambda::Client)
    @sqs = @aws.client(Aws::SQS::Client)
  end

  def create_function(name)
    @lambda.create_function(
      function_name: name,
      runtime: "ruby3.3",
      role: "arn:aws:iam::000000000000:role/lambda",
      handler: "app.handler",
      code: { zip_file: "PK fake zip" },
      environment: { variables: { "GREETING" => "hello" } }
    )
  end

  def test_invoke_runs_the_registered_handler
    url = @sqs.create_queue(queue_name: "seen").queue_url
    create_function("greet")
    @aws.register_function("greet") do |event, context|
      context.log("event #{event['n']}")
      @sqs.send_message(queue_url: url, message_body: JSON.generate(event))
      { "message" => "#{context.environment['GREETING']} from #{context.function_name}" }
    end

    out = @lambda.invoke(function_name: "greet", payload: '{"n":1}', log_type: "Tail")
    assert_equal 200, out.status_code
    assert_nil out.function_error
    assert_equal({ "message" => "hello from greet" }, JSON.parse(out.payload.read))
    assert_includes Base64.decode64(out.log_result), "event 1\nEND RequestId"
    assert_equal ['{"n":1}'], @sqs.receive_message(queue_url: url).messages.map(&:body)
  end

  def test_exceptions_are_function_errors
    create_function("boom")
    @aws.register_function("boom") { |_event, _context| raise ArgumentError, "bad input" }
    out = @lambda.invoke(function_name: "boom")
    assert_equal "Unhandled", out.function_error
    body = JSON.parse(out.payload.read)
    assert_equal "ArgumentError", body["errorType"]
    assert_equal "bad input", body["errorMessage"]

    create_function("unbound")
    out = @lambda.invoke(function_name: "unbound")
    assert_equal "Runtime.HandlerNotFound", JSON.parse(out.payload.read)["errorType"]
  end
end

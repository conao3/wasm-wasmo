# frozen_string_literal: true

require_relative "test_helper"
require "aws-sdk-lambda"
require "aws-sdk-states"

class StepFunctionsTest < Minitest::Test
  include WasmoTest

  ROLE = "arn:aws:iam::000000000000:role/sfn"

  def setup
    super
    @lambda = @aws.client(Aws::Lambda::Client)
    @sfn = @aws.client(Aws::States::Client)
  end

  def create_function(name)
    @lambda.create_function(
      function_name: name,
      runtime: "ruby3.3",
      role: "arn:aws:iam::000000000000:role/lambda",
      handler: "app.handler",
      code: { zip_file: "PK fake zip" }
    )
  end

  def invoke(function, payload)
    { "Type" => "Task", "Resource" => "arn:aws:states:::lambda:invoke",
      "Parameters" => { "FunctionName" => function }.merge(payload) }
  end

  def test_state_machine_retries_and_catches_handler_errors
    create_function("flaky")
    create_function("broken")
    calls = 0
    @aws.register_function("flaky") do |event, _context|
      calls += 1
      raise IOError, "not yet" if calls < 2

      { "doubled" => event["n"] * 2 }
    end
    @aws.register_function("broken") { |_event, _context| raise ArgumentError, "always" }
    definition = {
      "StartAt" => "Flaky",
      "States" => {
        "Flaky" => invoke("flaky", "Payload.$" => "$").merge(
          "ResultSelector" => { "doubled.$" => "$.Payload.doubled" },
          "ResultPath" => "$.result",
          "Retry" => [{ "ErrorEquals" => ["IOError"], "MaxAttempts" => 2 }],
          "Next" => "Broken"
        ),
        "Broken" => invoke("broken", "Payload" => {}).merge(
          "Catch" => [{ "ErrorEquals" => ["ArgumentError"], "ResultPath" => "$.caught", "Next" => "Done" }],
          "Next" => "Done"
        ),
        "Done" => { "Type" => "Succeed" }
      }
    }
    arn = @sfn.create_state_machine(name: "retry-catch", role_arn: ROLE,
                                    definition: JSON.generate(definition)).state_machine_arn
    execution = @sfn.start_execution(state_machine_arn: arn, input: '{"n":21}').execution_arn
    d = @sfn.describe_execution(execution_arn: execution)
    assert_equal "SUCCEEDED", d.status
    output = JSON.parse(d.output)
    assert_equal({ "doubled" => 42 }, output["result"])
    assert_equal "ArgumentError", output["caught"]["Error"]
    assert_equal 2, calls
  end

  def test_sync_execution_waits_for_the_handler
    create_function("greet")
    @aws.register_function("greet") { |event, _context| "hello #{event['name']}" }
    definition = {
      "StartAt" => "Greet",
      "States" => { "Greet" => invoke("greet", "Payload.$" => "$").merge("OutputPath" => "$.Payload", "End" => true) }
    }
    arn = @sfn.create_state_machine(name: "sync", role_arn: ROLE, type: "EXPRESS",
                                    definition: JSON.generate(definition)).state_machine_arn
    out = @sfn.start_sync_execution(state_machine_arn: arn, input: '{"name":"ann"}')
    assert_equal "SUCCEEDED", out.status
    assert_equal "hello ann", JSON.parse(out.output)
  end
end

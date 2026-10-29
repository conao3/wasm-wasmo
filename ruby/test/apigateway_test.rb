# frozen_string_literal: true

require_relative "test_helper"
require "aws-sdk-apigateway"
require "aws-sdk-lambda"

class ApiGatewayTest < Minitest::Test
  include WasmoTest

  def setup
    super
    @lambda = @aws.client(Aws::Lambda::Client)
    @apigw = @aws.client(Aws::APIGateway::Client)
  end

  def create_api(function_arn)
    api = @apigw.create_rest_api(name: "shop")
    pets = @apigw.create_resource(rest_api_id: api.id, parent_id: api.root_resource_id, path_part: "pets")
    pet = @apigw.create_resource(rest_api_id: api.id, parent_id: pets.id, path_part: "{id}")
    @apigw.put_method(rest_api_id: api.id, resource_id: pet.id, http_method: "GET", authorization_type: "NONE")
    @apigw.put_integration(
      rest_api_id: api.id, resource_id: pet.id, http_method: "GET", type: "AWS_PROXY",
      integration_http_method: "POST",
      uri: "arn:aws:apigateway:us-east-1:lambda:path/2015-03-31/functions/#{function_arn}/invocations"
    )
    @apigw.create_deployment(rest_api_id: api.id, stage_name: "prod", variables: { "env" => "test" })
    api.id
  end

  def test_rest_api_invokes_handler_through_execute_api
    fn = @lambda.create_function(
      function_name: "pets", runtime: "ruby3.3", role: "arn:aws:iam::000000000000:role/lambda",
      handler: "app.handler", code: { zip_file: "PK fake zip" }
    )
    @aws.register_function("pets") do |event, _context|
      id = event["pathParameters"]["id"]
      { "statusCode" => 200, "headers" => { "x-pet" => id },
        "body" => JSON.generate({ "id" => id, "env" => event["stageVariables"]["env"] }) }
    end
    base = "https://#{create_api(fn.function_arn)}.execute-api.us-east-1.amazonaws.com/prod"

    res = @aws.request("GET", "#{base}/pets/7")
    assert_equal 200, res.status
    assert_includes res.headers, %w[x-pet 7]
    assert_equal({ "id" => "7", "env" => "test" }, JSON.parse(res.body))

    missing = @aws.request("GET", "#{base}/nothing")
    assert_equal 403, missing.status
    assert_equal({ "message" => "Missing Authentication Token" }, JSON.parse(missing.body))
  end
end

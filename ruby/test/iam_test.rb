# frozen_string_literal: true

require_relative "test_helper"
require "aws-sdk-iam"

class IAMTest < Minitest::Test
  include WasmoTest

  TRUST = {
    Version: "2012-10-17",
    Statement: [{ Effect: "Allow", Principal: { Service: "lambda.amazonaws.com" }, Action: "sts:AssumeRole" }]
  }.freeze
  READ_ORDERS = {
    Version: "2012-10-17",
    Statement: [{ Effect: "Allow", Action: "dynamodb:GetItem", Resource: "arn:aws:dynamodb:*:*:table/orders" }]
  }.freeze

  def test_role_policy_and_simulation
    iam = @aws.client(Aws::IAM::Client)
    role = iam.create_role(role_name: "app", assume_role_policy_document: JSON.generate(TRUST)).role
    assert_equal "arn:aws:iam::000000000000:role/app", role.arn
    policy = iam.create_policy(policy_name: "read-orders", policy_document: JSON.generate(READ_ORDERS)).policy
    iam.attach_role_policy(role_name: "app", policy_arn: policy.arn)
    assert_equal ["read-orders"], iam.list_attached_role_policies(role_name: "app").attached_policies.map(&:policy_name)

    results = iam.simulate_principal_policy(
      policy_source_arn: role.arn,
      action_names: ["dynamodb:GetItem", "dynamodb:PutItem"],
      resource_arns: ["arn:aws:dynamodb:us-east-1:000000000000:table/orders"]
    ).evaluation_results
    assert_equal(
      { "dynamodb:GetItem" => "allowed", "dynamodb:PutItem" => "implicitDeny" },
      results.to_h { |r| [r.eval_action_name, r.eval_decision] }
    )
    assert_raises(Aws::IAM::Errors::NoSuchEntity) { iam.get_role(role_name: "missing") }
  end
end

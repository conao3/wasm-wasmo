# frozen_string_literal: true

require_relative "test_helper"
require "aws-sdk-sts"

class STSTest < Minitest::Test
  include WasmoTest

  def test_get_caller_identity
    out = @aws.client(Aws::STS::Client).get_caller_identity
    assert_equal "000000000000", out.account
  end

  def test_assume_role
    sts = @aws.client(Aws::STS::Client)
    out = sts.assume_role(role_arn: "arn:aws:iam::000000000000:role/app", role_session_name: "s")
    assert_match(/\AASIA/, out.credentials.access_key_id)
    assert_equal "arn:aws:sts::000000000000:assumed-role/app/s", out.assumed_role_user.arn
  end
end

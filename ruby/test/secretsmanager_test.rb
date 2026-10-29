# frozen_string_literal: true

require_relative "test_helper"
require "aws-sdk-secretsmanager"

class SecretsManagerTest < Minitest::Test
  include WasmoTest

  def test_versions_and_stages
    sm = @aws.client(Aws::SecretsManager::Client)
    first = sm.create_secret(name: "db", secret_string: "v1")
    second = sm.put_secret_value(secret_id: "db", secret_string: "v2")

    assert_equal "v2", sm.get_secret_value(secret_id: "db").secret_string
    previous = sm.get_secret_value(secret_id: "db", version_stage: "AWSPREVIOUS")
    assert_equal "v1", previous.secret_string
    assert_equal first.version_id, previous.version_id

    stages = sm.list_secret_version_ids(secret_id: "db").versions.to_h { |v| [v.version_id, v.version_stages] }
    assert_equal({ first.version_id => ["AWSPREVIOUS"], second.version_id => ["AWSCURRENT"] }, stages)

    sm.put_secret_value(secret_id: "db", secret_binary: "\x00\x01v3".b)
    assert_equal "\x00\x01v3".b, sm.get_secret_value(secret_id: "db").secret_binary.b
    assert_raises(Aws::SecretsManager::Errors::ResourceNotFoundException) do
      sm.get_secret_value(secret_id: "missing")
    end
  end
end

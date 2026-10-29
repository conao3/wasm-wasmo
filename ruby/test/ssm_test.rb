# frozen_string_literal: true

require_relative "test_helper"
require "aws-sdk-ssm"

class SSMTest < Minitest::Test
  include WasmoTest

  def test_get_parameters_by_path
    ssm = @aws.client(Aws::SSM::Client)
    %w[/app/db/host /app/db/port /app/api/url /other/x].each do |name|
      ssm.put_parameter(name: name, value: name.split("/").last, type: "String")
    end
    ssm.put_parameter(name: "/app/db/password", value: "hunter2", type: "SecureString")

    assert_empty ssm.get_parameters_by_path(path: "/app").parameters

    found = ssm.get_parameters_by_path(path: "/app", recursive: true, with_decryption: true, max_results: 2)
               .flat_map(&:parameters).to_h { |p| [p.name, p.value] }
    assert_equal(
      { "/app/db/host" => "host", "/app/db/port" => "port", "/app/db/password" => "hunter2", "/app/api/url" => "url" },
      found
    )

    db = ssm.get_parameters_by_path(path: "/app/db/").parameters
    assert_equal %w[/app/db/host /app/db/password /app/db/port], db.map(&:name).sort
    secret = db.find { |p| p.name == "/app/db/password" }
    assert_equal "SecureString", secret.type
    refute_equal "hunter2", secret.value
  end
end

# frozen_string_literal: true

require_relative "test_helper"
require "aws-sdk-cloudwatchlogs"

class LogsTest < Minitest::Test
  include WasmoTest

  def test_put_and_filter_log_events
    logs = @aws.client(Aws::CloudWatchLogs::Client)
    logs.create_log_group(log_group_name: "/app/api")
    %w[web-1 web-2].each { |s| logs.create_log_stream(log_group_name: "/app/api", log_stream_name: s) }
    now = (Time.now.to_f * 1000).to_i
    logs.put_log_events(
      log_group_name: "/app/api",
      log_stream_name: "web-1",
      log_events: [{ timestamp: now, message: "GET /health 200" }, { timestamp: now + 1, message: "GET /orders 500" }]
    )
    logs.put_log_events(
      log_group_name: "/app/api",
      log_stream_name: "web-2",
      log_events: [{ timestamp: now + 2, message: "POST /orders 500" }]
    )

    errors = logs.filter_log_events(log_group_name: "/app/api", filter_pattern: '"500"').events
    assert_equal([["web-1", "GET /orders 500"], ["web-2", "POST /orders 500"]],
                 errors.map { |e| [e.log_stream_name, e.message] })
    one = logs.filter_log_events(log_group_name: "/app/api", log_stream_names: ["web-1"]).events
    assert_equal ["GET /health 200", "GET /orders 500"], one.map(&:message)
    assert_raises(Aws::CloudWatchLogs::Errors::ResourceNotFoundException) do
      logs.put_log_events(log_group_name: "/app/api", log_stream_name: "missing",
                          log_events: [{ timestamp: now, message: "x" }])
    end
  end
end

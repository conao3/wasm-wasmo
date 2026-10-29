# frozen_string_literal: true

require_relative "test_helper"
require "aws-sdk-cloudwatch"

class CloudWatchTest < Minitest::Test
  include WasmoTest

  def test_put_and_get_metric_data
    cw = @aws.client(Aws::CloudWatch::Client)
    now = Time.now.utc
    base = Time.utc(now.year, now.month, now.day, now.hour) - 3600
    dims = [{ name: "Service", value: "web" }]
    data = [10.0, 20.0, 30.0].map { |v| { metric_name: "Latency", dimensions: dims, timestamp: base, value: v } }
    data << { metric_name: "Latency", dimensions: dims, timestamp: base + 60, value: 100.0, unit: "Milliseconds" }
    cw.put_metric_data(namespace: "App", metric_data: data)

    out = cw.get_metric_data(
      start_time: base,
      end_time: base + 3600,
      scan_by: "TimestampAscending",
      metric_data_queries: [
        {
          id: "sum",
          metric_stat: {
            metric: { namespace: "App", metric_name: "Latency", dimensions: dims }, period: 60, stat: "Sum"
          }
        },
        { id: "doubled", expression: "sum * 2", label: "doubled" }
      ]
    )
    results = out.metric_data_results.to_h { |r| [r.id, r] }
    assert_equal [60.0, 100.0], results["sum"].values
    assert_equal [base, base + 60], results["sum"].timestamps
    assert_equal [120.0, 200.0], results["doubled"].values
    assert_equal ["Latency"], cw.list_metrics(namespace: "App").metrics.map(&:metric_name)
  end
end

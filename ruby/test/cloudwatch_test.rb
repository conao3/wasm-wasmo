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
    assert_raises(Aws::CloudWatch::Errors::InvalidParameterValue) do
      cw.put_metric_data(namespace: "AWS/EC2", metric_data: [{ metric_name: "x", value: 1.0 }])
    end
  end

  def test_large_put_metric_data_is_gzipped
    cw = @aws.client(Aws::CloudWatch::Client)
    encodings = []
    cw.handle(step: :sign, priority: 0) do |context|
      encodings << context.http_request.headers["content-encoding"]
      @handler.call(context)
    end
    now = Time.now.utc
    at = Time.utc(now.year, now.month, now.day, now.hour)
    cw.put_metric_data(
      namespace: "Bulk",
      metric_data: (1..500).map do |i|
        dims = [{ name: "Host", value: "host-#{i % 10}" }]
        { metric_name: "Requests", dimensions: dims, timestamp: at, value: i.to_f }
      end
    )
    assert_equal ["gzip"], encodings

    stats = cw.get_metric_statistics(
      namespace: "Bulk", metric_name: "Requests", dimensions: [{ name: "Host", value: "host-0" }],
      start_time: at, end_time: at + 3600, period: 3600, statistics: %w[Sum SampleCount]
    ).datapoints
    assert_equal([[12_750.0, 50.0]], stats.map { |d| [d.sum, d.sample_count] })
  end
end

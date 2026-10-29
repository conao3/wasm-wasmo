package wasmo_test

import (
	"context"
	"encoding/json"
	"errors"
	"testing"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/cloudwatch"
	"github.com/aws/aws-sdk-go-v2/service/cloudwatch/types"
	"github.com/aws/aws-sdk-go-v2/service/cloudwatchlogs"
	logtypes "github.com/aws/aws-sdk-go-v2/service/cloudwatchlogs/types"
	"github.com/aws/aws-sdk-go-v2/service/sns"
	"github.com/aws/aws-sdk-go-v2/service/sqs"
)

func newCloudWatch(t *testing.T) *cloudwatch.Client {
	t.Helper()
	return cloudwatch.NewFromConfig(newMock(t).Config())
}

func putValues(t *testing.T, client *cloudwatch.Client, ns, name string, dims []types.Dimension, at time.Time, values ...float64) {
	t.Helper()
	var data []types.MetricDatum
	for _, v := range values {
		data = append(data, types.MetricDatum{MetricName: aws.String(name), Dimensions: dims, Timestamp: aws.Time(at), Value: aws.Float64(v), Unit: types.StandardUnitMilliseconds})
	}
	if _, err := client.PutMetricData(context.Background(), &cloudwatch.PutMetricDataInput{Namespace: aws.String(ns), MetricData: data}); err != nil {
		t.Fatal(err)
	}
}

func TestCloudWatchMetricsAndStatistics(t *testing.T) {
	ctx := context.Background()
	client := newCloudWatch(t)
	base := time.Now().Truncate(time.Hour).Add(-2 * time.Hour)
	web := []types.Dimension{{Name: aws.String("Service"), Value: aws.String("web")}}
	putValues(t, client, "App", "Latency", web, base.Add(10*time.Second), 10, 20, 30)
	putValues(t, client, "App", "Latency", web, base.Add(70*time.Second), 100)
	putValues(t, client, "App", "Latency", []types.Dimension{{Name: aws.String("Service"), Value: aws.String("api")}}, base, 1)
	if _, err := client.PutMetricData(ctx, &cloudwatch.PutMetricDataInput{Namespace: aws.String("App"), MetricData: []types.MetricDatum{{
		MetricName:      aws.String("Requests"),
		Timestamp:       aws.Time(base),
		StatisticValues: &types.StatisticSet{SampleCount: aws.Float64(4), Sum: aws.Float64(40), Minimum: aws.Float64(5), Maximum: aws.Float64(15)},
	}}}); err != nil {
		t.Fatal(err)
	}

	stats, err := client.GetMetricStatistics(ctx, &cloudwatch.GetMetricStatisticsInput{
		Namespace:          aws.String("App"),
		MetricName:         aws.String("Latency"),
		Dimensions:         web,
		StartTime:          aws.Time(base),
		EndTime:            aws.Time(base.Add(time.Hour)),
		Period:             aws.Int32(60),
		Statistics:         []types.Statistic{types.StatisticAverage, types.StatisticSum, types.StatisticSampleCount, types.StatisticMaximum},
		ExtendedStatistics: []string{"p50"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(stats.Datapoints) != 2 {
		t.Fatalf("datapoints = %+v", stats.Datapoints)
	}
	first := stats.Datapoints[0]
	if aws.ToFloat64(first.Average) != 20 || aws.ToFloat64(first.Sum) != 60 || aws.ToFloat64(first.SampleCount) != 3 || aws.ToFloat64(first.Maximum) != 30 || first.ExtendedStatistics["p50"] != 20 {
		t.Fatalf("first period = %+v", first)
	}
	if first.Unit != types.StandardUnitMilliseconds || !aws.ToTime(first.Timestamp).Equal(base) {
		t.Fatalf("first period unit/timestamp = %s %v", first.Unit, first.Timestamp)
	}
	hourly, err := client.GetMetricStatistics(ctx, &cloudwatch.GetMetricStatisticsInput{
		Namespace: aws.String("App"), MetricName: aws.String("Requests"),
		StartTime: aws.Time(base), EndTime: aws.Time(base.Add(time.Hour)), Period: aws.Int32(3600),
		Statistics: []types.Statistic{types.StatisticAverage, types.StatisticMinimum},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(hourly.Datapoints) != 1 || aws.ToFloat64(hourly.Datapoints[0].Average) != 10 || aws.ToFloat64(hourly.Datapoints[0].Minimum) != 5 {
		t.Fatalf("statistic set = %+v", hourly.Datapoints)
	}

	metrics, err := client.ListMetrics(ctx, &cloudwatch.ListMetricsInput{Namespace: aws.String("App"), Dimensions: []types.DimensionFilter{{Name: aws.String("Service")}}})
	if err != nil {
		t.Fatal(err)
	}
	if len(metrics.Metrics) != 2 {
		t.Fatalf("metrics = %+v", metrics.Metrics)
	}

	data, err := client.GetMetricData(ctx, &cloudwatch.GetMetricDataInput{
		StartTime: aws.Time(base),
		EndTime:   aws.Time(base.Add(time.Hour)),
		ScanBy:    types.ScanByTimestampAscending,
		MetricDataQueries: []types.MetricDataQuery{
			{Id: aws.String("web"), MetricStat: &types.MetricStat{Metric: &types.Metric{Namespace: aws.String("App"), MetricName: aws.String("Latency"), Dimensions: web}, Period: aws.Int32(60), Stat: aws.String("Sum")}},
			{Id: aws.String("api"), MetricStat: &types.MetricStat{Metric: &types.Metric{Namespace: aws.String("App"), MetricName: aws.String("Latency"), Dimensions: []types.Dimension{{Name: aws.String("Service"), Value: aws.String("api")}}}, Period: aws.Int32(60), Stat: aws.String("Sum")}, ReturnData: aws.Bool(false)},
			{Id: aws.String("total"), Expression: aws.String("(web + api) * 2"), Label: aws.String("doubled total")},
		},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(data.MetricDataResults) != 2 {
		t.Fatalf("results = %+v", data.MetricDataResults)
	}
	webResult, total := data.MetricDataResults[0], data.MetricDataResults[1]
	if aws.ToString(webResult.Label) != "Latency" || len(webResult.Values) != 2 || webResult.Values[0] != 60 || webResult.Values[1] != 100 {
		t.Fatalf("web = %+v", webResult)
	}
	if aws.ToString(total.Label) != "doubled total" || len(total.Values) != 1 || total.Values[0] != 122 {
		t.Fatalf("total = %+v", total)
	}

	var invalid *types.InvalidParameterValueException
	if _, err := client.PutMetricData(ctx, &cloudwatch.PutMetricDataInput{Namespace: aws.String("AWS/EC2"), MetricData: []types.MetricDatum{{MetricName: aws.String("x"), Value: aws.Float64(1)}}}); !errors.As(err, &invalid) {
		t.Fatalf("reserved namespace: %v", err)
	}
}

func TestCloudWatchAlarmsEvaluateAndNotify(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	client := cloudwatch.NewFromConfig(m.Config())
	snsClient := sns.NewFromConfig(m.Config())
	sqsClient := sqs.NewFromConfig(m.Config())
	topic := mustTopic(t, snsClient, "alarms", nil)
	url, queueArn := queueWithArn(t, sqsClient, "alarm-inbox", nil)
	subscribeQueue(t, snsClient, topic, queueArn, map[string]string{"RawMessageDelivery": "true"})

	dims := []types.Dimension{{Name: aws.String("Queue"), Value: aws.String("jobs")}}
	if _, err := client.PutMetricAlarm(ctx, &cloudwatch.PutMetricAlarmInput{
		AlarmName:          aws.String("backlog-high"),
		Namespace:          aws.String("Jobs"),
		MetricName:         aws.String("Backlog"),
		Dimensions:         dims,
		Statistic:          types.StatisticMaximum,
		Period:             aws.Int32(60),
		EvaluationPeriods:  aws.Int32(1),
		Threshold:          aws.Float64(100),
		ComparisonOperator: types.ComparisonOperatorGreaterThanThreshold,
		AlarmActions:       []string{topic},
		OKActions:          []string{topic},
		Tags:               []types.Tag{{Key: aws.String("team"), Value: aws.String("ops")}},
	}); err != nil {
		t.Fatal(err)
	}
	describe := func(name string) types.MetricAlarm {
		t.Helper()
		out, err := client.DescribeAlarms(ctx, &cloudwatch.DescribeAlarmsInput{AlarmNames: []string{name}})
		if err != nil {
			t.Fatal(err)
		}
		if len(out.MetricAlarms) != 1 {
			t.Fatalf("alarms = %+v", out.MetricAlarms)
		}
		return out.MetricAlarms[0]
	}
	if a := describe("backlog-high"); a.StateValue != types.StateValueInsufficientData {
		t.Fatalf("initial state = %s", a.StateValue)
	}

	putValues(t, client, "Jobs", "Backlog", dims, time.Now(), 50, 250)
	alarm := describe("backlog-high")
	if alarm.StateValue != types.StateValueAlarm {
		t.Fatalf("state after breach = %s (%s)", alarm.StateValue, aws.ToString(alarm.StateReason))
	}
	msgs := drain(t, sqsClient, url)
	if len(msgs) != 1 {
		t.Fatalf("notifications = %d", len(msgs))
	}
	var note struct {
		AlarmName      string
		NewStateValue  string
		OldStateValue  string
		NewStateReason string
		Trigger        struct{ MetricName, Namespace string }
	}
	if err := json.Unmarshal([]byte(aws.ToString(msgs[0].Body)), &note); err != nil {
		t.Fatal(err)
	}
	if note.AlarmName != "backlog-high" || note.NewStateValue != "ALARM" || note.OldStateValue != "INSUFFICIENT_DATA" || note.Trigger.MetricName != "Backlog" {
		t.Fatalf("notification = %+v", note)
	}

	if _, err := client.SetAlarmState(ctx, &cloudwatch.SetAlarmStateInput{AlarmName: aws.String("backlog-high"), StateValue: types.StateValueOk, StateReason: aws.String("manual reset")}); err != nil {
		t.Fatal(err)
	}
	if a := describe("backlog-high"); a.StateValue != types.StateValueOk || aws.ToString(a.StateReason) != "manual reset" {
		t.Fatalf("after SetAlarmState = %s %s", a.StateValue, aws.ToString(a.StateReason))
	}
	if len(drain(t, sqsClient, url)) != 1 {
		t.Fatal("OK action did not notify")
	}

	if _, err := client.PutCompositeAlarm(ctx, &cloudwatch.PutCompositeAlarmInput{AlarmName: aws.String("any-trouble"), AlarmRule: aws.String(`ALARM("backlog-high") OR ALARM("missing-alarm")`)}); err != nil {
		t.Fatal(err)
	}
	putValues(t, client, "Jobs", "Backlog", dims, time.Now(), 500)
	composite, err := client.DescribeAlarms(ctx, &cloudwatch.DescribeAlarmsInput{AlarmTypes: []types.AlarmType{types.AlarmTypeCompositeAlarm}})
	if err != nil {
		t.Fatal(err)
	}
	if len(composite.CompositeAlarms) != 1 || composite.CompositeAlarms[0].StateValue != types.StateValueAlarm {
		t.Fatalf("composite = %+v", composite.CompositeAlarms)
	}

	history, err := client.DescribeAlarmHistory(ctx, &cloudwatch.DescribeAlarmHistoryInput{AlarmName: aws.String("backlog-high"), HistoryItemType: types.HistoryItemTypeStateUpdate})
	if err != nil {
		t.Fatal(err)
	}
	if len(history.AlarmHistoryItems) != 3 {
		t.Fatalf("history = %+v", history.AlarmHistoryItems)
	}
	forMetric, err := client.DescribeAlarmsForMetric(ctx, &cloudwatch.DescribeAlarmsForMetricInput{Namespace: aws.String("Jobs"), MetricName: aws.String("Backlog"), Dimensions: dims})
	if err != nil {
		t.Fatal(err)
	}
	if len(forMetric.MetricAlarms) != 1 {
		t.Fatalf("alarms for metric = %+v", forMetric.MetricAlarms)
	}
	tags, err := client.ListTagsForResource(ctx, &cloudwatch.ListTagsForResourceInput{ResourceARN: alarm.AlarmArn})
	if err != nil {
		t.Fatal(err)
	}
	if len(tags.Tags) != 1 || aws.ToString(tags.Tags[0].Value) != "ops" {
		t.Fatalf("tags = %+v", tags.Tags)
	}

	var nf *types.ResourceNotFound
	if _, err := client.SetAlarmState(ctx, &cloudwatch.SetAlarmStateInput{AlarmName: aws.String("nope"), StateValue: types.StateValueAlarm, StateReason: aws.String("x")}); !errors.As(err, &nf) || httpStatus(err) != 404 {
		t.Fatalf("missing alarm: %v", err)
	}
	if _, err := client.DeleteAlarms(ctx, &cloudwatch.DeleteAlarmsInput{AlarmNames: []string{"any-trouble", "backlog-high"}}); err != nil {
		t.Fatal(err)
	}
}

func TestCloudWatchDashboards(t *testing.T) {
	ctx := context.Background()
	client := newCloudWatch(t)
	body := `{"widgets":[{"type":"text","properties":{"markdown":"hi"}}]}`
	if _, err := client.PutDashboard(ctx, &cloudwatch.PutDashboardInput{DashboardName: aws.String("ops"), DashboardBody: aws.String(body)}); err != nil {
		t.Fatal(err)
	}
	got, err := client.GetDashboard(ctx, &cloudwatch.GetDashboardInput{DashboardName: aws.String("ops")})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(got.DashboardBody) != body {
		t.Fatalf("body = %s", aws.ToString(got.DashboardBody))
	}
	list, err := client.ListDashboards(ctx, &cloudwatch.ListDashboardsInput{})
	if err != nil {
		t.Fatal(err)
	}
	if len(list.DashboardEntries) != 1 || aws.ToInt64(list.DashboardEntries[0].Size) != int64(len(body)) {
		t.Fatalf("dashboards = %+v", list.DashboardEntries)
	}
	var bad *types.DashboardInvalidInputError
	if _, err := client.PutDashboard(ctx, &cloudwatch.PutDashboardInput{DashboardName: aws.String("x"), DashboardBody: aws.String("not json")}); !errors.As(err, &bad) {
		t.Fatalf("invalid body: %v", err)
	}
	if _, err := client.DeleteDashboards(ctx, &cloudwatch.DeleteDashboardsInput{DashboardNames: []string{"ops"}}); err != nil {
		t.Fatal(err)
	}
	var nf *types.DashboardNotFoundError
	if _, err := client.GetDashboard(ctx, &cloudwatch.GetDashboardInput{DashboardName: aws.String("ops")}); !errors.As(err, &nf) {
		t.Fatalf("deleted dashboard: %v", err)
	}
}

func TestLogsMetricFiltersPublishToCloudWatch(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	logs := cloudwatchlogs.NewFromConfig(m.Config())
	cw := cloudwatch.NewFromConfig(m.Config())
	logGroup(t, logs, "api", "s")
	if _, err := logs.PutMetricFilter(ctx, &cloudwatchlogs.PutMetricFilterInput{
		LogGroupName:  aws.String("api"),
		FilterName:    aws.String("latency"),
		FilterPattern: aws.String(`{ $.latency > 0 }`),
		MetricTransformations: []logtypes.MetricTransformation{{
			MetricName:      aws.String("Latency"),
			MetricNamespace: aws.String("Api"),
			MetricValue:     aws.String("$.latency"),
			Dimensions:      map[string]string{"Route": "$.route"},
			Unit:            logtypes.StandardUnitMilliseconds,
		}},
	}); err != nil {
		t.Fatal(err)
	}
	now := time.Now()
	putLogs(t, logs, "api", "s", now.Add(-time.Minute), `{"route":"/a","latency":40}`, `{"route":"/a","latency":60}`, `{"route":"/b","latency":5}`, "not json")

	out, err := cw.GetMetricStatistics(ctx, &cloudwatch.GetMetricStatisticsInput{
		Namespace:  aws.String("Api"),
		MetricName: aws.String("Latency"),
		Dimensions: []types.Dimension{{Name: aws.String("Route"), Value: aws.String("/a")}},
		StartTime:  aws.Time(now.Add(-time.Hour)),
		EndTime:    aws.Time(now.Add(time.Hour)),
		Period:     aws.Int32(7200),
		Statistics: []types.Statistic{types.StatisticSum, types.StatisticSampleCount},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(out.Datapoints) != 1 || aws.ToFloat64(out.Datapoints[0].Sum) != 100 || aws.ToFloat64(out.Datapoints[0].SampleCount) != 2 || out.Datapoints[0].Unit != types.StandardUnitMilliseconds {
		t.Fatalf("datapoints = %+v", out.Datapoints)
	}
	filters, err := logs.DescribeMetricFilters(ctx, &cloudwatchlogs.DescribeMetricFiltersInput{LogGroupName: aws.String("api")})
	if err != nil {
		t.Fatal(err)
	}
	if len(filters.MetricFilters) != 1 || filters.MetricFilters[0].MetricTransformations[0].Dimensions["Route"] != "$.route" {
		t.Fatalf("filters = %+v", filters.MetricFilters)
	}
}

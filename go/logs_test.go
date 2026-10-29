package wasmo_test

import (
	"bytes"
	"compress/gzip"
	"context"
	"encoding/json"
	"errors"
	"io"
	"strings"
	"testing"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/cloudwatchlogs"
	"github.com/aws/aws-sdk-go-v2/service/cloudwatchlogs/types"
	"github.com/aws/aws-sdk-go-v2/service/firehose"
	firehosetypes "github.com/aws/aws-sdk-go-v2/service/firehose/types"
	"github.com/aws/aws-sdk-go-v2/service/kinesis"
	kinesistypes "github.com/aws/aws-sdk-go-v2/service/kinesis/types"
)

func logGroup(t *testing.T, client *cloudwatchlogs.Client, group string, streams ...string) {
	t.Helper()
	ctx := context.Background()
	if _, err := client.CreateLogGroup(ctx, &cloudwatchlogs.CreateLogGroupInput{LogGroupName: aws.String(group)}); err != nil {
		t.Fatal(err)
	}
	for _, s := range streams {
		if _, err := client.CreateLogStream(ctx, &cloudwatchlogs.CreateLogStreamInput{LogGroupName: aws.String(group), LogStreamName: aws.String(s)}); err != nil {
			t.Fatal(err)
		}
	}
}

func putLogs(t *testing.T, client *cloudwatchlogs.Client, group, stream string, start time.Time, messages ...string) {
	t.Helper()
	var events []types.InputLogEvent
	for i, m := range messages {
		events = append(events, types.InputLogEvent{Timestamp: aws.Int64(start.Add(time.Duration(i) * time.Second).UnixMilli()), Message: aws.String(m)})
	}
	if _, err := client.PutLogEvents(context.Background(), &cloudwatchlogs.PutLogEventsInput{LogGroupName: aws.String(group), LogStreamName: aws.String(stream), LogEvents: events}); err != nil {
		t.Fatal(err)
	}
}

func messages(events []types.FilteredLogEvent) []string {
	var out []string
	for _, e := range events {
		out = append(out, aws.ToString(e.Message))
	}
	return out
}

type awslogsPayload struct {
	MessageType         string   `json:"messageType"`
	Owner               string   `json:"owner"`
	LogGroup            string   `json:"logGroup"`
	LogStream           string   `json:"logStream"`
	SubscriptionFilters []string `json:"subscriptionFilters"`
	LogEvents           []struct {
		ID        string `json:"id"`
		Timestamp int64  `json:"timestamp"`
		Message   string `json:"message"`
	} `json:"logEvents"`
}

func gunzipJSON(t *testing.T, data []byte, v any) {
	t.Helper()
	r, err := gzip.NewReader(bytes.NewReader(data))
	if err != nil {
		t.Fatal(err)
	}
	plain, err := io.ReadAll(r)
	if err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(plain, v); err != nil {
		t.Fatalf("%v: %s", err, plain)
	}
}

func TestLogsGroupsStreamsAndEvents(t *testing.T) {
	ctx := context.Background()
	client := cloudwatchlogs.NewFromConfig(newMock(t).Config())
	logGroup(t, client, "/app/api", "web-1", "web-2")
	logGroup(t, client, "/app/worker")

	groups, err := client.DescribeLogGroups(ctx, &cloudwatchlogs.DescribeLogGroupsInput{LogGroupNamePrefix: aws.String("/app/")})
	if err != nil {
		t.Fatal(err)
	}
	if len(groups.LogGroups) != 2 || !strings.HasSuffix(aws.ToString(groups.LogGroups[0].Arn), ":log-group:/app/api:*") {
		t.Fatalf("groups = %+v", groups.LogGroups)
	}
	var exists *types.ResourceAlreadyExistsException
	if _, err := client.CreateLogGroup(ctx, &cloudwatchlogs.CreateLogGroupInput{LogGroupName: aws.String("/app/api")}); !errors.As(err, &exists) {
		t.Fatalf("duplicate group: %v", err)
	}

	start := time.Now().Add(-time.Minute)
	putLogs(t, client, "/app/api", "web-1", start, "first", "second", "third")
	putLogs(t, client, "/app/api", "web-2", start.Add(500*time.Millisecond), "other")

	head, err := client.GetLogEvents(ctx, &cloudwatchlogs.GetLogEventsInput{LogGroupName: aws.String("/app/api"), LogStreamName: aws.String("web-1"), StartFromHead: aws.Bool(true), Limit: aws.Int32(2)})
	if err != nil {
		t.Fatal(err)
	}
	if len(head.Events) != 2 || aws.ToString(head.Events[0].Message) != "first" {
		t.Fatalf("head = %+v", head.Events)
	}
	next, err := client.GetLogEvents(ctx, &cloudwatchlogs.GetLogEventsInput{LogGroupName: aws.String("/app/api"), LogStreamName: aws.String("web-1"), NextToken: head.NextForwardToken, Limit: aws.Int32(2)})
	if err != nil {
		t.Fatal(err)
	}
	if len(next.Events) != 1 || aws.ToString(next.Events[0].Message) != "third" {
		t.Fatalf("next page = %+v", next.Events)
	}

	all, err := client.FilterLogEvents(ctx, &cloudwatchlogs.FilterLogEventsInput{LogGroupName: aws.String("/app/api")})
	if err != nil {
		t.Fatal(err)
	}
	if got := strings.Join(messages(all.Events), ","); got != "first,other,second,third" {
		t.Fatalf("interleaved = %s", got)
	}

	streams, err := client.DescribeLogStreams(ctx, &cloudwatchlogs.DescribeLogStreamsInput{LogGroupName: aws.String("/app/api")})
	if err != nil {
		t.Fatal(err)
	}
	if len(streams.LogStreams) != 2 || aws.ToInt64(streams.LogStreams[0].FirstEventTimestamp) != start.UnixMilli() {
		t.Fatalf("streams = %+v", streams.LogStreams)
	}

	_, err = client.PutLogEvents(ctx, &cloudwatchlogs.PutLogEventsInput{
		LogGroupName:  aws.String("/app/api"),
		LogStreamName: aws.String("web-1"),
		LogEvents: []types.InputLogEvent{
			{Timestamp: aws.Int64(start.Add(time.Hour).UnixMilli()), Message: aws.String("late")},
			{Timestamp: aws.Int64(start.UnixMilli()), Message: aws.String("early")},
		},
	})
	var invalid *types.InvalidParameterException
	if !errors.As(err, &invalid) {
		t.Fatalf("out-of-order batch: %v", err)
	}
	var nf *types.ResourceNotFoundException
	if _, err := client.PutLogEvents(ctx, &cloudwatchlogs.PutLogEventsInput{LogGroupName: aws.String("/app/api"), LogStreamName: aws.String("missing"), LogEvents: []types.InputLogEvent{{Timestamp: aws.Int64(start.UnixMilli()), Message: aws.String("x")}}}); !errors.As(err, &nf) {
		t.Fatalf("missing stream: %v", err)
	}

	if _, err := client.PutRetentionPolicy(ctx, &cloudwatchlogs.PutRetentionPolicyInput{LogGroupName: aws.String("/app/api"), RetentionInDays: aws.Int32(7)}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.TagResource(ctx, &cloudwatchlogs.TagResourceInput{ResourceArn: groups.LogGroups[0].LogGroupArn, Tags: map[string]string{"team": "api"}}); err != nil {
		t.Fatal(err)
	}
	tags, err := client.ListTagsForResource(ctx, &cloudwatchlogs.ListTagsForResourceInput{ResourceArn: groups.LogGroups[0].LogGroupArn})
	if err != nil {
		t.Fatal(err)
	}
	if tags.Tags["team"] != "api" {
		t.Fatalf("tags = %v", tags.Tags)
	}
	described, err := client.DescribeLogGroups(ctx, &cloudwatchlogs.DescribeLogGroupsInput{LogGroupNamePrefix: aws.String("/app/api")})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToInt32(described.LogGroups[0].RetentionInDays) != 7 {
		t.Fatalf("retention = %v", described.LogGroups[0].RetentionInDays)
	}
	if _, err := client.DeleteLogGroup(ctx, &cloudwatchlogs.DeleteLogGroupInput{LogGroupName: aws.String("/app/worker")}); err != nil {
		t.Fatal(err)
	}
}

func TestLogsFilterPatterns(t *testing.T) {
	ctx := context.Background()
	client := cloudwatchlogs.NewFromConfig(newMock(t).Config())
	logGroup(t, client, "app", "s")
	putLogs(t, client, "app", "s", time.Now().Add(-time.Minute),
		"ERROR db timeout",
		"WARN slow query",
		"INFO ok",
		`{"level":"error","latency":250,"user":{"id":7}}`,
		`{"level":"info","latency":20,"user":{"id":8}}`,
		`10.0.0.1 - - [10/Oct/2000:13:55:36 -0700] "GET /a HTTP/1.0" 503 120`,
		`10.0.0.2 - - [10/Oct/2000:13:55:37 -0700] "GET /b HTTP/1.0" 200 2326`,
	)
	for pattern, want := range map[string]string{
		"ERROR":                                 "ERROR db timeout",
		"?ERROR ?WARN":                          "ERROR db timeout|WARN slow query",
		"ERROR -timeout":                        "",
		`{ $.latency > 100 }`:                   `{"level":"error","latency":250,"user":{"id":7}}`,
		`{ $.level = "info" && $.user.id = 8 }`: `{"level":"info","latency":20,"user":{"id":8}}`,
		`[ip, id, user, ts, request, status = 5*, bytes]`: `10.0.0.1 - - [10/Oct/2000:13:55:36 -0700] "GET /a HTTP/1.0" 503 120`,
		`[..., status = 200, bytes > 1000]`:               `10.0.0.2 - - [10/Oct/2000:13:55:37 -0700] "GET /b HTTP/1.0" 200 2326`,
		`%sl[o]w%`:                                        "WARN slow query",
	} {
		out, err := client.FilterLogEvents(ctx, &cloudwatchlogs.FilterLogEventsInput{LogGroupName: aws.String("app"), FilterPattern: aws.String(pattern)})
		if err != nil {
			t.Fatalf("%s: %v", pattern, err)
		}
		if got := strings.Join(messages(out.Events), "|"); got != want {
			t.Fatalf("%s matched %q, want %q", pattern, got, want)
		}
	}
	var invalid *types.InvalidParameterException
	if _, err := client.FilterLogEvents(ctx, &cloudwatchlogs.FilterLogEventsInput{LogGroupName: aws.String("app"), FilterPattern: aws.String("{ $.a = ")}); !errors.As(err, &invalid) {
		t.Fatalf("malformed pattern: %v", err)
	}
	tested, err := client.TestMetricFilter(ctx, &cloudwatchlogs.TestMetricFilterInput{
		FilterPattern:    aws.String("[ip, id, user, ts, request, status, bytes]"),
		LogEventMessages: []string{`1.2.3.4 - - [x] "GET /" 200 10`, "too short"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(tested.Matches) != 1 || tested.Matches[0].ExtractedValues["$status"] != "200" {
		t.Fatalf("matches = %+v", tested.Matches)
	}
}

func TestLogsSubscriptionDeliversToKinesis(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	logs := cloudwatchlogs.NewFromConfig(m.Config())
	kin := kinesis.NewFromConfig(m.Config())
	createStream(t, kin, "log-sink", 1)
	desc, err := kin.DescribeStreamSummary(ctx, &kinesis.DescribeStreamSummaryInput{StreamName: aws.String("log-sink")})
	if err != nil {
		t.Fatal(err)
	}
	logGroup(t, logs, "/svc/orders", "i-123")
	if _, err := logs.PutSubscriptionFilter(ctx, &cloudwatchlogs.PutSubscriptionFilterInput{
		LogGroupName:   aws.String("/svc/orders"),
		FilterName:     aws.String("errors"),
		FilterPattern:  aws.String("ERROR"),
		DestinationArn: desc.StreamDescriptionSummary.StreamARN,
		RoleArn:        aws.String("arn:aws:iam::000000000000:role/cwl"),
	}); err != nil {
		t.Fatal(err)
	}
	start := time.Now().Add(-time.Minute)
	putLogs(t, logs, "/svc/orders", "i-123", start, "INFO started", "ERROR card declined", "ERROR stock missing")
	putLogs(t, logs, "/svc/orders", "i-123", start.Add(time.Second*10), "INFO only info")

	records, _ := readShard(t, kin, "log-sink", "shardId-000000000000", kinesistypes.ShardIteratorTypeTrimHorizon, nil)
	if len(records) != 1 {
		t.Fatalf("records = %d, want one per matching PutLogEvents", len(records))
	}
	if aws.ToString(records[0].PartitionKey) != "i-123" {
		t.Fatalf("partition key = %q", aws.ToString(records[0].PartitionKey))
	}
	var payload awslogsPayload
	gunzipJSON(t, records[0].Data, &payload)
	if payload.MessageType != "DATA_MESSAGE" || payload.Owner != "000000000000" || payload.LogGroup != "/svc/orders" || payload.LogStream != "i-123" {
		t.Fatalf("payload = %+v", payload)
	}
	if len(payload.SubscriptionFilters) != 1 || payload.SubscriptionFilters[0] != "errors" {
		t.Fatalf("filters = %v", payload.SubscriptionFilters)
	}
	if len(payload.LogEvents) != 2 || payload.LogEvents[0].Message != "ERROR card declined" || payload.LogEvents[1].Timestamp != start.Add(2*time.Second).UnixMilli() || payload.LogEvents[0].ID == "" {
		t.Fatalf("events = %+v", payload.LogEvents)
	}

	filters, err := logs.DescribeSubscriptionFilters(ctx, &cloudwatchlogs.DescribeSubscriptionFiltersInput{LogGroupName: aws.String("/svc/orders")})
	if err != nil {
		t.Fatal(err)
	}
	if len(filters.SubscriptionFilters) != 1 || aws.ToString(filters.SubscriptionFilters[0].DestinationArn) != aws.ToString(desc.StreamDescriptionSummary.StreamARN) {
		t.Fatalf("filters = %+v", filters.SubscriptionFilters)
	}
}

func TestLogsSubscriptionThroughFirehoseAndExportToS3(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	logs := cloudwatchlogs.NewFromConfig(m.Config())
	fh := s3Delivery(t, m, "log-archive", "archive", "cwl/", firehosetypes.CompressionFormatUncompressed)
	stream, err := fh.DescribeDeliveryStream(ctx, &firehose.DescribeDeliveryStreamInput{DeliveryStreamName: aws.String("log-archive")})
	if err != nil {
		t.Fatal(err)
	}
	logGroup(t, logs, "audit", "main")
	if _, err := logs.PutDestination(ctx, &cloudwatchlogs.PutDestinationInput{
		DestinationName: aws.String("central"),
		TargetArn:       stream.DeliveryStreamDescription.DeliveryStreamARN,
		RoleArn:         aws.String("arn:aws:iam::000000000000:role/cwl"),
	}); err != nil {
		t.Fatal(err)
	}
	dests, err := logs.DescribeDestinations(ctx, &cloudwatchlogs.DescribeDestinationsInput{})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := logs.PutSubscriptionFilter(ctx, &cloudwatchlogs.PutSubscriptionFilterInput{
		LogGroupName:   aws.String("audit"),
		FilterName:     aws.String("all"),
		FilterPattern:  aws.String(""),
		DestinationArn: dests.Destinations[0].Arn,
	}); err != nil {
		t.Fatal(err)
	}
	start := time.Now().Add(-time.Minute)
	putLogs(t, logs, "audit", "main", start, "login alice", "logout alice")

	s3c := s3Client(m, true)
	objects := objectsUnder(t, s3c, "archive", "cwl/")
	if len(objects) != 1 {
		t.Fatalf("objects = %d", len(objects))
	}
	for _, body := range objects {
		var payload awslogsPayload
		gunzipJSON(t, body, &payload)
		if len(payload.LogEvents) != 2 || payload.LogGroup != "audit" {
			t.Fatalf("payload = %+v", payload)
		}
	}

	task, err := logs.CreateExportTask(ctx, &cloudwatchlogs.CreateExportTaskInput{
		LogGroupName:      aws.String("audit"),
		From:              aws.Int64(start.Add(-time.Second).UnixMilli()),
		To:                aws.Int64(time.Now().UnixMilli()),
		Destination:       aws.String("archive"),
		DestinationPrefix: aws.String("exports"),
	})
	if err != nil {
		t.Fatal(err)
	}
	tasks, err := logs.DescribeExportTasks(ctx, &cloudwatchlogs.DescribeExportTasksInput{TaskId: task.TaskId})
	if err != nil {
		t.Fatal(err)
	}
	if len(tasks.ExportTasks) != 1 || tasks.ExportTasks[0].Status.Code != types.ExportTaskStatusCodeCompleted {
		t.Fatalf("tasks = %+v", tasks.ExportTasks)
	}
	exported := objectsUnder(t, s3c, "archive", "exports/"+aws.ToString(task.TaskId)+"/main/")
	if len(exported) != 1 {
		t.Fatalf("exported = %v", exported)
	}
	for _, body := range exported {
		r, err := gzip.NewReader(bytes.NewReader(body))
		if err != nil {
			t.Fatal(err)
		}
		plain, _ := io.ReadAll(r)
		if lines := strings.Split(strings.TrimSpace(string(plain)), "\n"); len(lines) != 2 || !strings.HasSuffix(lines[0], " login alice") {
			t.Fatalf("export = %q", plain)
		}
	}
}

func TestLogsInsightsQueries(t *testing.T) {
	ctx := context.Background()
	client := cloudwatchlogs.NewFromConfig(newMock(t).Config())
	logGroup(t, client, "api", "a", "b")
	start := time.Now().Add(-time.Minute)
	putLogs(t, client, "api", "a", start, `{"route":"/users","status":200,"ms":12}`, `{"route":"/users","status":500,"ms":80}`)
	putLogs(t, client, "api", "b", start.Add(time.Second/2), `{"route":"/orders","status":200,"ms":30}`, "plain text line")

	run := func(query string) [][]types.ResultField {
		t.Helper()
		started, err := client.StartQuery(ctx, &cloudwatchlogs.StartQueryInput{
			LogGroupName: aws.String("api"),
			QueryString:  aws.String(query),
			StartTime:    aws.Int64(start.Add(-time.Minute).Unix()),
			EndTime:      aws.Int64(time.Now().Add(time.Minute).Unix()),
		})
		if err != nil {
			t.Fatal(err)
		}
		for range 10 {
			out, err := client.GetQueryResults(ctx, &cloudwatchlogs.GetQueryResultsInput{QueryId: started.QueryId})
			if err != nil {
				t.Fatal(err)
			}
			if out.Status == types.QueryStatusComplete {
				return out.Results
			}
		}
		t.Fatal("query did not complete")
		return nil
	}
	value := func(row []types.ResultField, field string) string {
		for _, f := range row {
			if aws.ToString(f.Field) == field {
				return aws.ToString(f.Value)
			}
		}
		return ""
	}

	rows := run("fields @timestamp, route, ms | filter status = 200 | sort ms desc")
	if len(rows) != 2 || value(rows[0], "route") != "/orders" || value(rows[1], "ms") != "12" {
		t.Fatalf("rows = %+v", rows)
	}
	stats := run("stats count(*) as n, avg(ms) as avg_ms by route | sort n desc")
	if len(stats) != 3 || value(stats[0], "route") != "/users" || value(stats[0], "n") != "2" || value(stats[0], "avg_ms") != "46" {
		t.Fatalf("stats = %+v", stats)
	}
	text := run("fields @message, @logStream | filter @message like /plain/")
	if len(text) != 1 || value(text[0], "@logStream") != "b" {
		t.Fatalf("like = %+v", text)
	}
	var malformed *types.MalformedQueryException
	if _, err := client.StartQuery(ctx, &cloudwatchlogs.StartQueryInput{LogGroupName: aws.String("api"), QueryString: aws.String("explode now"), StartTime: aws.Int64(0), EndTime: aws.Int64(1)}); !errors.As(err, &malformed) {
		t.Fatalf("malformed query: %v", err)
	}
}

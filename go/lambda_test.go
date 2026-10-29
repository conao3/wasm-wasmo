package wasmo_test

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/cloudwatchlogs"
	"github.com/aws/aws-sdk-go-v2/service/dynamodb"
	ddbtypes "github.com/aws/aws-sdk-go-v2/service/dynamodb/types"
	"github.com/aws/aws-sdk-go-v2/service/eventbridge"
	ebtypes "github.com/aws/aws-sdk-go-v2/service/eventbridge/types"
	"github.com/aws/aws-sdk-go-v2/service/kinesis"
	"github.com/aws/aws-sdk-go-v2/service/lambda"
	"github.com/aws/aws-sdk-go-v2/service/lambda/types"
	"github.com/aws/aws-sdk-go-v2/service/s3"
	s3types "github.com/aws/aws-sdk-go-v2/service/s3/types"
	"github.com/aws/aws-sdk-go-v2/service/sns"
	"github.com/aws/aws-sdk-go-v2/service/sqs"
	sqstypes "github.com/aws/aws-sdk-go-v2/service/sqs/types"
	wasmo "github.com/conao3/wasm-wasmo/go"
)

func createFunction(t *testing.T, c *lambda.Client, name string, env map[string]string) string {
	t.Helper()
	in := &lambda.CreateFunctionInput{
		FunctionName: aws.String(name),
		Runtime:      types.RuntimeProvidedal2023,
		Role:         aws.String("arn:aws:iam::000000000000:role/lambda"),
		Handler:      aws.String("bootstrap"),
		Code:         &types.FunctionCode{ZipFile: []byte("PK fake zip")},
	}
	if env != nil {
		in.Environment = &types.Environment{Variables: env}
	}
	out, err := c.CreateFunction(context.Background(), in)
	if err != nil {
		t.Fatal(err)
	}
	return aws.ToString(out.FunctionArn)
}

// recorder collects the events a function receives.
type recorder struct {
	mu     sync.Mutex
	events []json.RawMessage
}

func (r *recorder) handler(result string) wasmo.Handler {
	return func(_ context.Context, event json.RawMessage) (json.RawMessage, error) {
		r.mu.Lock()
		defer r.mu.Unlock()
		r.events = append(r.events, event)
		return json.RawMessage(result), nil
	}
}

func (r *recorder) all() []json.RawMessage {
	r.mu.Lock()
	defer r.mu.Unlock()
	return slices.Clone(r.events)
}

func TestLambdaInvoke(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	c := lambda.NewFromConfig(m.Config())
	createFunction(t, c, "greet", map[string]string{"GREETING": "hello"})
	m.RegisterFunction("greet", func(ctx context.Context, event json.RawMessage) (json.RawMessage, error) {
		fc, _ := wasmo.FunctionContextFrom(ctx)
		var in struct{ Name string }
		if err := json.Unmarshal(event, &in); err != nil {
			return nil, err
		}
		if in.Name == "" {
			return nil, errors.New("name is required")
		}
		fc.Logf("greeting %s", in.Name)
		return json.Marshal(map[string]string{
			"message": fc.Environment["GREETING"] + ", " + in.Name,
			"region":  fc.Environment["AWS_REGION"],
			"arn":     fc.InvokedFunctionArn,
		})
	})

	out, err := c.Invoke(ctx, &lambda.InvokeInput{
		FunctionName: aws.String("greet"),
		Payload:      []byte(`{"Name":"ada"}`),
		LogType:      types.LogTypeTail,
	})
	if err != nil {
		t.Fatal(err)
	}
	var got map[string]string
	if err := json.Unmarshal(out.Payload, &got); err != nil {
		t.Fatal(err)
	}
	if got["message"] != "hello, ada" || got["region"] != "us-east-1" || !strings.HasSuffix(got["arn"], ":function:greet") {
		t.Fatalf("payload = %s", out.Payload)
	}
	if out.StatusCode != 200 || out.FunctionError != nil || aws.ToString(out.ExecutedVersion) != "$LATEST" {
		t.Fatalf("invoke = %+v", out)
	}
	log, _ := base64.StdEncoding.DecodeString(aws.ToString(out.LogResult))
	if !strings.Contains(string(log), "greeting ada\nEND RequestId:") {
		t.Fatalf("log = %q", log)
	}

	failed, err := c.Invoke(ctx, &lambda.InvokeInput{FunctionName: aws.String("greet"), Payload: []byte(`{}`)})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(failed.FunctionError) != "Unhandled" || string(failed.Payload) != `{"errorMessage":"name is required","errorType":"errorString"}` {
		t.Fatalf("function error = %v %s", aws.ToString(failed.FunctionError), failed.Payload)
	}

	createFunction(t, c, "unbound", nil)
	unbound, err := c.Invoke(ctx, &lambda.InvokeInput{FunctionName: aws.String("unbound")})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(unbound.FunctionError) != "Unhandled" || !strings.Contains(string(unbound.Payload), "Runtime.HandlerNotFound") {
		t.Fatalf("unbound = %s", unbound.Payload)
	}

	_, err = c.Invoke(ctx, &lambda.InvokeInput{FunctionName: aws.String("missing")})
	var nf *types.ResourceNotFoundException
	if !errors.As(err, &nf) {
		t.Fatalf("missing function: %v", err)
	}

	dry, err := c.Invoke(ctx, &lambda.InvokeInput{FunctionName: aws.String("greet"), InvocationType: types.InvocationTypeDryRun})
	if err != nil || dry.StatusCode != 204 {
		t.Fatalf("dry run = %+v %v", dry, err)
	}

	if _, err := c.PutFunctionConcurrency(ctx, &lambda.PutFunctionConcurrencyInput{
		FunctionName: aws.String("greet"), ReservedConcurrentExecutions: aws.Int32(0),
	}); err != nil {
		t.Fatal(err)
	}
	_, err = c.Invoke(ctx, &lambda.InvokeInput{FunctionName: aws.String("greet"), Payload: []byte(`{"Name":"x"}`)})
	var throttled *types.TooManyRequestsException
	if !errors.As(err, &throttled) {
		t.Fatalf("throttled invoke: %v", err)
	}
}

func TestLambdaHandlerCallsTheSameMock(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	c := lambda.NewFromConfig(m.Config())
	queues := sqs.NewFromConfig(m.Config())
	url, _ := queueWithArn(t, queues, "outbox", nil)
	createFunction(t, c, "forward", nil)
	m.RegisterFunction("forward", func(ctx context.Context, event json.RawMessage) (json.RawMessage, error) {
		if _, err := queues.SendMessage(ctx, &sqs.SendMessageInput{QueueUrl: aws.String(url), MessageBody: aws.String(string(event))}); err != nil {
			return nil, err
		}
		return json.RawMessage(`"sent"`), nil
	})
	out, err := c.Invoke(ctx, &lambda.InvokeInput{FunctionName: aws.String("forward"), Payload: []byte(`{"id":7}`)})
	if err != nil || string(out.Payload) != `"sent"` {
		t.Fatalf("invoke = %s %v", out.Payload, err)
	}
	got, err := queues.ReceiveMessage(ctx, &sqs.ReceiveMessageInput{QueueUrl: aws.String(url)})
	if err != nil || len(got.Messages) != 1 || aws.ToString(got.Messages[0].Body) != `{"id":7}` {
		t.Fatalf("receive = %+v %v", got, err)
	}
}

func TestLambdaVersionsAndAliases(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	c := lambda.NewFromConfig(m.Config())
	createFunction(t, c, "app", map[string]string{"STAGE": "one"})
	m.RegisterFunction("app", func(ctx context.Context, _ json.RawMessage) (json.RawMessage, error) {
		fc, _ := wasmo.FunctionContextFrom(ctx)
		return json.Marshal(fc.FunctionVersion + "/" + fc.Environment["STAGE"])
	})
	v1, err := c.PublishVersion(ctx, &lambda.PublishVersionInput{FunctionName: aws.String("app"), Description: aws.String("first")})
	if err != nil || aws.ToString(v1.Version) != "1" || !strings.HasSuffix(aws.ToString(v1.FunctionArn), ":app:1") {
		t.Fatalf("publish = %+v %v", v1, err)
	}
	if _, err := c.UpdateFunctionConfiguration(ctx, &lambda.UpdateFunctionConfigurationInput{
		FunctionName: aws.String("app"),
		Environment:  &types.Environment{Variables: map[string]string{"STAGE": "two"}},
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := c.PublishVersion(ctx, &lambda.PublishVersionInput{FunctionName: aws.String("app")}); err != nil {
		t.Fatal(err)
	}
	alias, err := c.CreateAlias(ctx, &lambda.CreateAliasInput{
		FunctionName: aws.String("app"), Name: aws.String("live"), FunctionVersion: aws.String("1"),
	})
	if err != nil || !strings.HasSuffix(aws.ToString(alias.AliasArn), ":app:live") {
		t.Fatalf("alias = %+v %v", alias, err)
	}

	invoke := func(name string) (string, string) {
		t.Helper()
		out, err := c.Invoke(ctx, &lambda.InvokeInput{FunctionName: aws.String(name)})
		if err != nil {
			t.Fatal(err)
		}
		var s string
		if err := json.Unmarshal(out.Payload, &s); err != nil {
			t.Fatalf("payload %s: %v", out.Payload, err)
		}
		return s, aws.ToString(out.ExecutedVersion)
	}
	if got, ver := invoke("app:live"); got != "1/one" || ver != "1" {
		t.Fatalf("live = %s (%s)", got, ver)
	}
	if got, _ := invoke("app"); got != "$LATEST/two" {
		t.Fatalf("latest = %s", got)
	}
	if got, _ := invoke("app:2"); got != "2/two" {
		t.Fatalf("version 2 = %s", got)
	}

	if _, err := c.UpdateAlias(ctx, &lambda.UpdateAliasInput{
		FunctionName: aws.String("app"),
		Name:         aws.String("live"),
		RoutingConfig: &types.AliasRoutingConfiguration{
			AdditionalVersionWeights: map[string]float64{"2": 1.0},
		},
	}); err != nil {
		t.Fatal(err)
	}
	if got, ver := invoke("app:live"); got != "2/two" || ver != "2" {
		t.Fatalf("weighted alias = %s (%s)", got, ver)
	}

	versions, err := c.ListVersionsByFunction(ctx, &lambda.ListVersionsByFunctionInput{FunctionName: aws.String("app")})
	if err != nil || len(versions.Versions) != 3 {
		t.Fatalf("versions = %+v %v", versions, err)
	}
	got, err := c.GetFunction(ctx, &lambda.GetFunctionInput{FunctionName: aws.String("app"), Qualifier: aws.String("live")})
	if err != nil || aws.ToString(got.Configuration.Version) != "1" || got.Configuration.Environment.Variables["STAGE"] != "one" {
		t.Fatalf("get alias = %+v %v", got, err)
	}
	if _, err := c.GetAlias(ctx, &lambda.GetAliasInput{FunctionName: aws.String("app"), Name: aws.String("gone")}); err == nil {
		t.Fatal("missing alias resolved")
	}
	if _, err := c.DeleteFunction(ctx, &lambda.DeleteFunctionInput{FunctionName: aws.String("app"), Qualifier: aws.String("1")}); err == nil {
		t.Fatal("deleted a version an alias points at")
	}

	if _, err := c.AddPermission(ctx, &lambda.AddPermissionInput{
		FunctionName: aws.String("app"),
		StatementId:  aws.String("sns"),
		Action:       aws.String("lambda:InvokeFunction"),
		Principal:    aws.String("sns.amazonaws.com"),
		SourceArn:    aws.String("arn:aws:sns:us-east-1:000000000000:events"),
	}); err != nil {
		t.Fatal(err)
	}
	policy, err := c.GetPolicy(ctx, &lambda.GetPolicyInput{FunctionName: aws.String("app")})
	if err != nil || !strings.Contains(aws.ToString(policy.Policy), `"AWS:SourceArn":"arn:aws:sns:us-east-1:000000000000:events"`) {
		t.Fatalf("policy = %v %v", aws.ToString(policy.Policy), err)
	}
	if _, err := c.RemovePermission(ctx, &lambda.RemovePermissionInput{FunctionName: aws.String("app"), StatementId: aws.String("sns")}); err != nil {
		t.Fatal(err)
	}
}

type sqsLambdaEvent struct {
	Records []struct {
		MessageID      string `json:"messageId"`
		Body           string `json:"body"`
		EventSource    string `json:"eventSource"`
		EventSourceARN string `json:"eventSourceARN"`
	}
}

func TestLambdaSQSEventSourceMapping(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	c := lambda.NewFromConfig(m.Config())
	queues := sqs.NewFromConfig(m.Config())
	url, arn := queueWithArn(t, queues, "jobs", map[string]string{"VisibilityTimeout": "60"})
	createFunction(t, c, "worker", nil)

	var mu sync.Mutex
	var bodies []string
	var batches int
	m.RegisterFunction("worker", func(_ context.Context, event json.RawMessage) (json.RawMessage, error) {
		var e sqsLambdaEvent
		if err := json.Unmarshal(event, &e); err != nil {
			return nil, err
		}
		mu.Lock()
		defer mu.Unlock()
		batches++
		var failures []map[string]string
		for _, r := range e.Records {
			if r.EventSource != "aws:sqs" || r.EventSourceARN != arn {
				return nil, errors.New("unexpected record source")
			}
			bodies = append(bodies, r.Body)
			if r.Body == "poison" {
				failures = append(failures, map[string]string{"itemIdentifier": r.MessageID})
			}
		}
		return json.Marshal(map[string]any{"batchItemFailures": failures})
	})

	for _, body := range []string{"a", "b", "poison"} {
		if _, err := queues.SendMessage(ctx, &sqs.SendMessageInput{QueueUrl: aws.String(url), MessageBody: aws.String(body)}); err != nil {
			t.Fatal(err)
		}
	}
	esm, err := c.CreateEventSourceMapping(ctx, &lambda.CreateEventSourceMappingInput{
		FunctionName:          aws.String("worker"),
		EventSourceArn:        aws.String(arn),
		BatchSize:             aws.Int32(2),
		FunctionResponseTypes: []types.FunctionResponseType{types.FunctionResponseTypeReportBatchItemFailures},
	})
	if err != nil {
		t.Fatal(err)
	}
	mu.Lock()
	if !slices.Equal(bodies, []string{"a", "b", "poison"}) || batches != 2 {
		t.Fatalf("bodies = %v in %d batches", bodies, batches)
	}
	mu.Unlock()
	attrs, err := queues.GetQueueAttributes(ctx, &sqs.GetQueueAttributesInput{
		QueueUrl:       aws.String(url),
		AttributeNames: []sqstypes.QueueAttributeName{sqstypes.QueueAttributeNameAll},
	})
	if err != nil {
		t.Fatal(err)
	}
	if attrs.Attributes["ApproximateNumberOfMessages"] != "0" || attrs.Attributes["ApproximateNumberOfMessagesNotVisible"] != "1" {
		t.Fatalf("queue = %v", attrs.Attributes)
	}

	if _, err := c.UpdateEventSourceMapping(ctx, &lambda.UpdateEventSourceMappingInput{UUID: esm.UUID, Enabled: aws.Bool(false)}); err != nil {
		t.Fatal(err)
	}
	if _, err := queues.SendMessage(ctx, &sqs.SendMessageInput{QueueUrl: aws.String(url), MessageBody: aws.String("later")}); err != nil {
		t.Fatal(err)
	}
	got, err := c.GetEventSourceMapping(ctx, &lambda.GetEventSourceMappingInput{UUID: esm.UUID})
	if err != nil || aws.ToString(got.State) != "Disabled" || aws.ToInt32(got.BatchSize) != 2 {
		t.Fatalf("mapping = %+v %v", got, err)
	}
	mu.Lock()
	if len(bodies) != 3 {
		t.Fatalf("disabled mapping consumed %v", bodies)
	}
	mu.Unlock()
	list, err := c.ListEventSourceMappings(ctx, &lambda.ListEventSourceMappingsInput{FunctionName: aws.String("worker")})
	if err != nil || len(list.EventSourceMappings) != 1 {
		t.Fatalf("list = %+v %v", list, err)
	}
	if _, err := c.DeleteEventSourceMapping(ctx, &lambda.DeleteEventSourceMappingInput{UUID: esm.UUID}); err != nil {
		t.Fatal(err)
	}
}

func TestLambdaStreamEventSourceMappings(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	c := lambda.NewFromConfig(m.Config())

	ks := kinesis.NewFromConfig(m.Config())
	if _, err := ks.CreateStream(ctx, &kinesis.CreateStreamInput{StreamName: aws.String("clicks"), ShardCount: aws.Int32(1)}); err != nil {
		t.Fatal(err)
	}
	stream, err := ks.DescribeStreamSummary(ctx, &kinesis.DescribeStreamSummaryInput{StreamName: aws.String("clicks")})
	if err != nil {
		t.Fatal(err)
	}
	db := dynamodb.NewFromConfig(m.Config())
	tableStream := streamTable(t, db, "users", ddbtypes.StreamViewTypeNewImage)

	createFunction(t, c, "clicks", nil)
	createFunction(t, c, "users", nil)
	var clicks, users recorder
	m.RegisterFunction("clicks", clicks.handler(`null`))
	m.RegisterFunction("users", users.handler(`null`))
	for fn, source := range map[string]string{"clicks": aws.ToString(stream.StreamDescriptionSummary.StreamARN), "users": tableStream} {
		if _, err := c.CreateEventSourceMapping(ctx, &lambda.CreateEventSourceMappingInput{
			FunctionName:     aws.String(fn),
			EventSourceArn:   aws.String(source),
			StartingPosition: types.EventSourcePositionLatest,
		}); err != nil {
			t.Fatal(err)
		}
	}

	if _, err := ks.PutRecord(ctx, &kinesis.PutRecordInput{
		StreamName: aws.String("clicks"), PartitionKey: aws.String("u1"), Data: []byte(`{"page":"/"}`),
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := db.PutItem(ctx, &dynamodb.PutItemInput{TableName: aws.String("users"), Item: map[string]ddbtypes.AttributeValue{"id": s("u1")}}); err != nil {
		t.Fatal(err)
	}

	var kev struct {
		Records []struct {
			EventSource string `json:"eventSource"`
			Kinesis     struct {
				PartitionKey string `json:"partitionKey"`
				Data         []byte `json:"data"`
			} `json:"kinesis"`
		}
	}
	if got := clicks.all(); len(got) != 1 || json.Unmarshal(got[0], &kev) != nil || kev.Records[0].EventSource != "aws:kinesis" ||
		kev.Records[0].Kinesis.PartitionKey != "u1" || string(kev.Records[0].Kinesis.Data) != `{"page":"/"}` {
		t.Fatalf("kinesis events = %s", got)
	}
	var dev struct {
		Records []struct {
			EventName      string `json:"eventName"`
			EventSourceARN string `json:"eventSourceARN"`
			Dynamodb       struct {
				Keys map[string]map[string]string
			} `json:"dynamodb"`
		}
	}
	if got := users.all(); len(got) != 1 || json.Unmarshal(got[0], &dev) != nil || dev.Records[0].EventName != "INSERT" ||
		dev.Records[0].EventSourceARN != tableStream || dev.Records[0].Dynamodb.Keys["id"]["S"] != "u1" {
		t.Fatalf("dynamodb events = %s", got)
	}
}

func TestLambdaSNSSubscription(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	c := lambda.NewFromConfig(m.Config())
	fn := createFunction(t, c, "notify", nil)
	var r recorder
	m.RegisterFunction("notify", r.handler(`null`))
	topics := sns.NewFromConfig(m.Config())
	topic, err := topics.CreateTopic(ctx, &sns.CreateTopicInput{Name: aws.String("orders")})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := topics.Subscribe(ctx, &sns.SubscribeInput{TopicArn: topic.TopicArn, Protocol: aws.String("lambda"), Endpoint: aws.String(fn)}); err != nil {
		t.Fatal(err)
	}
	if _, err := topics.Publish(ctx, &sns.PublishInput{TopicArn: topic.TopicArn, Message: aws.String("order 1"), Subject: aws.String("new")}); err != nil {
		t.Fatal(err)
	}
	var e struct {
		Records []struct {
			EventSource string
			Sns         struct {
				TopicArn string
				Subject  string
				Message  string
			}
		}
	}
	got := r.all()
	if len(got) != 1 || json.Unmarshal(got[0], &e) != nil || e.Records[0].EventSource != "aws:sns" ||
		e.Records[0].Sns.Message != "order 1" || e.Records[0].Sns.TopicArn != aws.ToString(topic.TopicArn) {
		t.Fatalf("events = %s", got)
	}
}

func TestLambdaEventBridgeTarget(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	c := lambda.NewFromConfig(m.Config())
	fn := createFunction(t, c, "audit", nil)
	var r recorder
	m.RegisterFunction("audit", r.handler(`null`))
	eb := eventbridge.NewFromConfig(m.Config())
	if _, err := eb.PutRule(ctx, &eventbridge.PutRuleInput{
		Name: aws.String("orders"), EventPattern: aws.String(`{"source":["shop"]}`),
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := eb.PutTargets(ctx, &eventbridge.PutTargetsInput{
		Rule: aws.String("orders"), Targets: []ebtypes.Target{{Id: aws.String("fn"), Arn: aws.String(fn)}},
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := eb.PutEvents(ctx, &eventbridge.PutEventsInput{Entries: []ebtypes.PutEventsRequestEntry{
		{Source: aws.String("shop"), DetailType: aws.String("Order Placed"), Detail: aws.String(`{"id":"o1"}`)},
		{Source: aws.String("other"), DetailType: aws.String("Ignored"), Detail: aws.String(`{}`)},
	}}); err != nil {
		t.Fatal(err)
	}
	var e struct {
		Source     string `json:"source"`
		DetailType string `json:"detail-type"`
		Detail     struct{ ID string }
	}
	got := r.all()
	if len(got) != 1 || json.Unmarshal(got[0], &e) != nil || e.Source != "shop" || e.DetailType != "Order Placed" || e.Detail.ID != "o1" {
		t.Fatalf("events = %s", got)
	}
}

func TestLambdaS3Notification(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	c := lambda.NewFromConfig(m.Config())
	fn := createFunction(t, c, "thumbnail", nil)
	var r recorder
	m.RegisterFunction("thumbnail", r.handler(`null`))
	bucket := s3Client(m, true)
	mustBucket(t, bucket, "photos")
	if _, err := bucket.PutBucketNotificationConfiguration(ctx, &s3.PutBucketNotificationConfigurationInput{
		Bucket: aws.String("photos"),
		NotificationConfiguration: &s3types.NotificationConfiguration{LambdaFunctionConfigurations: []s3types.LambdaFunctionConfiguration{{
			LambdaFunctionArn: aws.String(fn),
			Events:            []s3types.Event{s3types.EventS3ObjectCreated},
		}}},
	}); err != nil {
		t.Fatal(err)
	}
	putString(t, bucket, "photos", "cat.png", "meow")
	got := r.all()
	var events []s3Event
	for _, raw := range got {
		var e s3Event
		if err := json.Unmarshal(raw, &e); err != nil {
			t.Fatal(err)
		}
		events = append(events, e)
	}
	if len(events) != 1 || events[0].Records[0].EventName != "ObjectCreated:Put" || events[0].Records[0].S3.Object.Key != "cat.png" {
		t.Fatalf("events = %s", got)
	}
}

func TestLambdaAsyncInvokeDestinations(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	c := lambda.NewFromConfig(m.Config())
	queues := sqs.NewFromConfig(m.Config())
	failURL, failArn := queueWithArn(t, queues, "failures", nil)
	okURL, okArn := queueWithArn(t, queues, "successes", nil)
	createFunction(t, c, "job", nil)
	var mu sync.Mutex
	attempts := 0
	m.RegisterFunction("job", func(_ context.Context, event json.RawMessage) (json.RawMessage, error) {
		mu.Lock()
		defer mu.Unlock()
		attempts++
		if strings.Contains(string(event), "fail") {
			return nil, errors.New("job failed")
		}
		return json.RawMessage(`{"done":true}`), nil
	})
	if _, err := c.PutFunctionEventInvokeConfig(ctx, &lambda.PutFunctionEventInvokeConfigInput{
		FunctionName:         aws.String("job"),
		MaximumRetryAttempts: aws.Int32(1),
		DestinationConfig: &types.DestinationConfig{
			OnFailure: &types.OnFailure{Destination: aws.String(failArn)},
			OnSuccess: &types.OnSuccess{Destination: aws.String(okArn)},
		},
	}); err != nil {
		t.Fatal(err)
	}
	for _, payload := range []string{`{"fail":1}`, `{"ok":1}`} {
		out, err := c.Invoke(ctx, &lambda.InvokeInput{
			FunctionName: aws.String("job"), InvocationType: types.InvocationTypeEvent, Payload: []byte(payload),
		})
		if err != nil || out.StatusCode != 202 {
			t.Fatalf("invoke = %+v %v", out, err)
		}
	}
	if attempts != 3 {
		t.Fatalf("attempts = %d", attempts)
	}
	type record struct {
		RequestContext struct {
			Condition              string
			ApproximateInvokeCount int
		} `json:"requestContext"`
		RequestPayload  map[string]int `json:"requestPayload"`
		ResponsePayload map[string]any `json:"responsePayload"`
	}
	receive := func(url string) record {
		t.Helper()
		out, err := queues.ReceiveMessage(ctx, &sqs.ReceiveMessageInput{QueueUrl: aws.String(url)})
		if err != nil || len(out.Messages) != 1 {
			t.Fatalf("receive = %+v %v", out, err)
		}
		var r record
		if err := json.Unmarshal([]byte(aws.ToString(out.Messages[0].Body)), &r); err != nil {
			t.Fatal(err)
		}
		return r
	}
	failed := receive(failURL)
	if failed.RequestContext.Condition != "RetriesExhausted" || failed.RequestContext.ApproximateInvokeCount != 2 ||
		failed.RequestPayload["fail"] != 1 || failed.ResponsePayload["errorMessage"] != "job failed" {
		t.Fatalf("failure record = %+v", failed)
	}
	succeeded := receive(okURL)
	if succeeded.RequestContext.Condition != "Success" || succeeded.ResponsePayload["done"] != true {
		t.Fatalf("success record = %+v", succeeded)
	}
}

func TestLambdaControlPlane(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	c := lambda.NewFromConfig(m.Config())
	arn := createFunction(t, c, "ops", nil)
	if _, err := c.CreateFunction(ctx, &lambda.CreateFunctionInput{
		FunctionName: aws.String("ops"), Runtime: types.RuntimeNodejs20x, Role: aws.String("arn:aws:iam::000000000000:role/r"),
		Handler: aws.String("index.handler"), Code: &types.FunctionCode{ZipFile: []byte("x")},
	}); err == nil {
		t.Fatal("duplicate function created")
	}
	waiter := lambda.NewFunctionActiveV2Waiter(c)
	if err := waiter.Wait(ctx, &lambda.GetFunctionInput{FunctionName: aws.String("ops")}, 5e9); err != nil {
		t.Fatal(err)
	}
	if _, err := c.TagResource(ctx, &lambda.TagResourceInput{Resource: aws.String(arn), Tags: map[string]string{"team": "core"}}); err != nil {
		t.Fatal(err)
	}
	tags, err := c.ListTags(ctx, &lambda.ListTagsInput{Resource: aws.String(arn)})
	if err != nil || tags.Tags["team"] != "core" {
		t.Fatalf("tags = %+v %v", tags, err)
	}
	updated, err := c.UpdateFunctionCode(ctx, &lambda.UpdateFunctionCodeInput{FunctionName: aws.String("ops"), ZipFile: []byte("new code")})
	if err != nil || updated.CodeSize != int64(len("new code")) {
		t.Fatalf("update code = %+v %v", updated, err)
	}
	url, err := c.CreateFunctionUrlConfig(ctx, &lambda.CreateFunctionUrlConfigInput{FunctionName: aws.String("ops"), AuthType: types.FunctionUrlAuthTypeNone})
	if err != nil || !strings.HasSuffix(aws.ToString(url.FunctionUrl), ".lambda-url.us-east-1.on.aws/") {
		t.Fatalf("url = %+v %v", url, err)
	}
	layer, err := c.PublishLayerVersion(ctx, &lambda.PublishLayerVersionInput{
		LayerName: aws.String("deps"), Content: &types.LayerVersionContentInput{ZipFile: []byte("layer")},
		CompatibleRuntimes: []types.Runtime{types.RuntimeNodejs20x},
	})
	if err != nil || layer.Version != 1 {
		t.Fatalf("layer = %+v %v", layer, err)
	}
	cfg, err := c.UpdateFunctionConfiguration(ctx, &lambda.UpdateFunctionConfigurationInput{
		FunctionName: aws.String("ops"), Layers: []string{aws.ToString(layer.LayerVersionArn)}, Timeout: aws.Int32(30),
	})
	if err != nil || len(cfg.Layers) != 1 || cfg.Layers[0].CodeSize != 5 || aws.ToInt32(cfg.Timeout) != 30 {
		t.Fatalf("config = %+v %v", cfg, err)
	}
	list, err := c.ListFunctions(ctx, &lambda.ListFunctionsInput{})
	if err != nil || len(list.Functions) != 1 {
		t.Fatalf("list = %+v %v", list, err)
	}
	if _, err := c.DeleteFunction(ctx, &lambda.DeleteFunctionInput{FunctionName: aws.String("ops")}); err != nil {
		t.Fatal(err)
	}
	_, err = c.GetFunction(ctx, &lambda.GetFunctionInput{FunctionName: aws.String("ops")})
	if httpStatus(err) != 404 {
		t.Fatalf("deleted function: %v", err)
	}
}

func TestLambdaLogsSubscription(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	c := lambda.NewFromConfig(m.Config())
	fn := createFunction(t, c, "alerts", nil)
	var r recorder
	m.RegisterFunction("alerts", r.handler(`null`))
	logs := cloudwatchlogs.NewFromConfig(m.Config())
	logGroup(t, logs, "/svc/api", "web")
	if _, err := logs.PutSubscriptionFilter(ctx, &cloudwatchlogs.PutSubscriptionFilterInput{
		LogGroupName:   aws.String("/svc/api"),
		FilterName:     aws.String("errors"),
		FilterPattern:  aws.String("ERROR"),
		DestinationArn: aws.String(fn),
	}); err != nil {
		t.Fatal(err)
	}
	putLogs(t, logs, "/svc/api", "web", time.Now().Add(-time.Minute), "INFO ok", "ERROR broken")
	got := r.all()
	var e struct {
		Awslogs struct{ Data []byte } `json:"awslogs"`
	}
	if len(got) != 1 || json.Unmarshal(got[0], &e) != nil {
		t.Fatalf("events = %s", got)
	}
	var payload awslogsPayload
	gunzipJSON(t, e.Awslogs.Data, &payload)
	if payload.LogGroup != "/svc/api" || len(payload.LogEvents) != 1 || payload.LogEvents[0].Message != "ERROR broken" {
		t.Fatalf("payload = %+v", payload)
	}
}

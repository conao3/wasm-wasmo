package wasmo_test

import (
	"context"
	"encoding/json"
	"strings"
	"testing"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/eventbridge"
	"github.com/aws/aws-sdk-go-v2/service/eventbridge/types"
	"github.com/aws/aws-sdk-go-v2/service/sns"
	"github.com/aws/aws-sdk-go-v2/service/sqs"
)

func putRule(t *testing.T, client *eventbridge.Client, name, pattern string) string {
	t.Helper()
	out, err := client.PutRule(context.Background(), &eventbridge.PutRuleInput{Name: aws.String(name), EventPattern: aws.String(pattern)})
	if err != nil {
		t.Fatal(err)
	}
	return aws.ToString(out.RuleArn)
}

func putEvent(t *testing.T, client *eventbridge.Client, source, detailType, detail string) string {
	t.Helper()
	out, err := client.PutEvents(context.Background(), &eventbridge.PutEventsInput{Entries: []types.PutEventsRequestEntry{{
		Source:     aws.String(source),
		DetailType: aws.String(detailType),
		Detail:     aws.String(detail),
	}}})
	if err != nil {
		t.Fatal(err)
	}
	if out.FailedEntryCount != 0 {
		t.Fatalf("PutEvents failed: %+v", out.Entries)
	}
	return aws.ToString(out.Entries[0].EventId)
}

func TestEventBridgeRules(t *testing.T) {
	ctx := context.Background()
	client := eventbridge.NewFromConfig(newMock(t).Config())

	if _, err := client.PutRule(ctx, &eventbridge.PutRuleInput{Name: aws.String("bad"), EventPattern: aws.String(`{"source": "app"}`)}); apiCode(err) != "InvalidEventPatternException" {
		t.Fatalf("invalid pattern err = %v", err)
	}
	if _, err := client.PutRule(ctx, &eventbridge.PutRuleInput{Name: aws.String("none")}); apiCode(err) != "ValidationException" {
		t.Fatalf("rule without pattern err = %v", err)
	}
	arn := putRule(t, client, "orders", `{"source": ["shop"]}`)
	if arn != "arn:aws:events:us-east-1:000000000000:rule/orders" {
		t.Fatalf("rule arn = %s", arn)
	}
	if _, err := client.PutRule(ctx, &eventbridge.PutRuleInput{Name: aws.String("tick"), ScheduleExpression: aws.String("rate(5 minutes)"), State: types.RuleStateDisabled}); err != nil {
		t.Fatal(err)
	}
	rule, err := client.DescribeRule(ctx, &eventbridge.DescribeRuleInput{Name: aws.String("tick")})
	if err != nil {
		t.Fatal(err)
	}
	if rule.State != types.RuleStateDisabled || aws.ToString(rule.ScheduleExpression) != "rate(5 minutes)" || aws.ToString(rule.EventBusName) != "default" {
		t.Fatalf("rule = %+v", rule)
	}
	rules, err := client.ListRules(ctx, &eventbridge.ListRulesInput{NamePrefix: aws.String("ord")})
	if err != nil || len(rules.Rules) != 1 {
		t.Fatalf("rules = %+v %v", rules, err)
	}
	if _, err := client.DescribeRule(ctx, &eventbridge.DescribeRuleInput{Name: aws.String("missing")}); apiCode(err) != "ResourceNotFoundException" {
		t.Fatalf("missing rule err = %v", err)
	}

	if _, err := client.PutTargets(ctx, &eventbridge.PutTargetsInput{Rule: aws.String("orders"), Targets: []types.Target{{Id: aws.String("q"), Arn: aws.String("arn:aws:sqs:us-east-1:000000000000:q")}}}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.DeleteRule(ctx, &eventbridge.DeleteRuleInput{Name: aws.String("orders")}); apiCode(err) != "ValidationException" {
		t.Fatalf("delete rule with targets err = %v", err)
	}
	names, err := client.ListRuleNamesByTarget(ctx, &eventbridge.ListRuleNamesByTargetInput{TargetArn: aws.String("arn:aws:sqs:us-east-1:000000000000:q")})
	if err != nil || len(names.RuleNames) != 1 || names.RuleNames[0] != "orders" {
		t.Fatalf("rule names = %+v %v", names, err)
	}
	if _, err := client.RemoveTargets(ctx, &eventbridge.RemoveTargetsInput{Rule: aws.String("orders"), Ids: []string{"q"}}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.DeleteRule(ctx, &eventbridge.DeleteRuleInput{Name: aws.String("orders")}); err != nil {
		t.Fatal(err)
	}

	test := func(pattern, event string) bool {
		t.Helper()
		out, err := client.TestEventPattern(ctx, &eventbridge.TestEventPatternInput{EventPattern: aws.String(pattern), Event: aws.String(event)})
		if err != nil {
			t.Fatal(err)
		}
		return out.Result
	}
	event := `{"id": "1", "account": "000000000000", "source": "shop", "time": "2026-01-01T00:00:00Z", "region": "us-east-1", "resources": [], "detail-type": "Order", "detail": {"total": 120, "state": "paid", "sku": "BOOK-9", "ip": "10.0.0.5"}}`
	cases := []struct {
		pattern string
		want    bool
	}{
		{`{"source": ["shop"]}`, true},
		{`{"source": [{"prefix": "sh"}]}`, true},
		{`{"source": [{"anything-but": ["shop"]}]}`, false},
		{`{"detail": {"total": [{"numeric": [">", 100, "<=", 200]}]}}`, true},
		{`{"detail": {"total": [{"numeric": ["<", 100]}]}}`, false},
		{`{"detail": {"coupon": [{"exists": false}]}}`, true},
		{`{"detail": {"state": [{"exists": true}], "sku": [{"suffix": "-9"}]}}`, true},
		{`{"detail": {"state": [{"equals-ignore-case": "PAID"}]}}`, true},
		{`{"detail": {"ip": [{"cidr": "10.0.0.0/30"}]}}`, false},
		{`{"$or": [{"source": ["nope"]}, {"detail-type": ["Order"]}]}`, true},
	}
	for _, c := range cases {
		if got := test(c.pattern, event); got != c.want {
			t.Errorf("TestEventPattern(%s) = %v, want %v", c.pattern, got, c.want)
		}
	}
}

func TestEventBridgeToSQSAndSNS(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	client := eventbridge.NewFromConfig(m.Config())
	queues := sqs.NewFromConfig(m.Config())
	topics := sns.NewFromConfig(m.Config())

	fullURL, fullArn := queueWithArn(t, queues, "full", nil)
	shapedURL, shapedArn := queueWithArn(t, queues, "shaped", nil)
	fifoURL, fifoArn := queueWithArn(t, queues, "orders.fifo", map[string]string{"FifoQueue": "true", "ContentBasedDeduplication": "true"})
	viaTopicURL, viaTopicArn := queueWithArn(t, queues, "via-topic", nil)
	topic := mustTopic(t, topics, "alerts", nil)
	subscribeQueue(t, topics, topic, viaTopicArn, map[string]string{"RawMessageDelivery": "true"})

	ruleArn := putRule(t, client, "big-orders", `{"source": ["shop"], "detail": {"total": [{"numeric": [">=", 100]}]}}`)
	if _, err := client.PutTargets(ctx, &eventbridge.PutTargetsInput{Rule: aws.String("big-orders"), Targets: []types.Target{
		{Id: aws.String("full"), Arn: aws.String(fullArn)},
		{Id: aws.String("shaped"), Arn: aws.String(shapedArn), InputTransformer: &types.InputTransformer{
			InputPathsMap: map[string]string{"id": "$.detail.id", "total": "$.detail.total"},
			InputTemplate: aws.String(`{"order": "<id>", "total": <total>, "rule": "<aws.events.rule-name>"}`),
		}},
		{Id: aws.String("fifo"), Arn: aws.String(fifoArn), InputPath: aws.String("$.detail"), SqsParameters: &types.SqsParameters{MessageGroupId: aws.String("orders")}},
		{Id: aws.String("topic"), Arn: aws.String(topic), Input: aws.String(`"big order"`)},
	}}); err != nil {
		t.Fatal(err)
	}
	failed, err := client.PutTargets(ctx, &eventbridge.PutTargetsInput{Rule: aws.String("big-orders"), Targets: []types.Target{
		{Id: aws.String("nogroup"), Arn: aws.String(fifoArn)},
	}})
	if err != nil || failed.FailedEntryCount != 1 {
		t.Fatalf("fifo target without group = %+v %v", failed, err)
	}
	targets, err := client.ListTargetsByRule(ctx, &eventbridge.ListTargetsByRuleInput{Rule: aws.String("big-orders")})
	if err != nil || len(targets.Targets) != 4 {
		t.Fatalf("targets = %+v %v", targets, err)
	}

	id := putEvent(t, client, "shop", "Order Placed", `{"id": "o-1", "total": 150}`)
	putEvent(t, client, "shop", "Order Placed", `{"id": "o-2", "total": 20}`)
	putEvent(t, client, "other", "Order Placed", `{"id": "o-3", "total": 500}`)

	full := drain(t, queues, fullURL)
	if len(full) != 1 {
		t.Fatalf("full queue got %d", len(full))
	}
	var event struct {
		ID         string `json:"id"`
		Source     string `json:"source"`
		DetailType string `json:"detail-type"`
		Account    string `json:"account"`
		Region     string `json:"region"`
		Time       string `json:"time"`
		Detail     struct {
			ID    string `json:"id"`
			Total int    `json:"total"`
		} `json:"detail"`
	}
	if err := json.Unmarshal([]byte(aws.ToString(full[0].Body)), &event); err != nil {
		t.Fatal(err)
	}
	if event.ID != id || event.Source != "shop" || event.DetailType != "Order Placed" || event.Account != "000000000000" || event.Region != "us-east-1" || event.Detail.ID != "o-1" {
		t.Fatalf("event = %+v", event)
	}
	if _, err := time.Parse(time.RFC3339, event.Time); err != nil {
		t.Fatalf("event time %q: %v", event.Time, err)
	}

	shaped := drain(t, queues, shapedURL)
	if len(shaped) != 1 || aws.ToString(shaped[0].Body) != `{"order": "o-1", "total": 150, "rule": "big-orders"}` {
		t.Fatalf("transformed = %+v", shaped)
	}
	fifo := drain(t, queues, fifoURL)
	if len(fifo) != 1 || aws.ToString(fifo[0].Body) != `{"id":"o-1","total":150}` {
		t.Fatalf("input path = %+v", fifo)
	}
	viaTopic := drain(t, queues, viaTopicURL)
	if len(viaTopic) != 1 || aws.ToString(viaTopic[0].Body) != `"big order"` {
		t.Fatalf("via topic = %+v", viaTopic)
	}

	if _, err := client.DisableRule(ctx, &eventbridge.DisableRuleInput{Name: aws.String("big-orders")}); err != nil {
		t.Fatal(err)
	}
	putEvent(t, client, "shop", "Order Placed", `{"id": "o-4", "total": 150}`)
	if got := drain(t, queues, fullURL); len(got) != 0 {
		t.Fatalf("disabled rule delivered %+v", got)
	}
	if !strings.HasSuffix(ruleArn, "/big-orders") {
		t.Fatalf("rule arn = %s", ruleArn)
	}

	bad, err := client.PutEvents(ctx, &eventbridge.PutEventsInput{Entries: []types.PutEventsRequestEntry{
		{Source: aws.String("shop"), DetailType: aws.String("x"), Detail: aws.String("not json")},
		{Source: aws.String("shop"), DetailType: aws.String("x"), Detail: aws.String("{}"), EventBusName: aws.String("missing")},
	}})
	if err != nil {
		t.Fatal(err)
	}
	if bad.FailedEntryCount != 2 || aws.ToString(bad.Entries[0].ErrorCode) != "MalformedDetail" {
		t.Fatalf("bad entries = %+v", bad.Entries)
	}
}

func TestEventBridgeBusesArchivesReplays(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	client := eventbridge.NewFromConfig(m.Config())
	queues := sqs.NewFromConfig(m.Config())

	bus, err := client.CreateEventBus(ctx, &eventbridge.CreateEventBusInput{Name: aws.String("orders"), Description: aws.String("order events")})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := client.CreateEventBus(ctx, &eventbridge.CreateEventBusInput{Name: aws.String("orders")}); apiCode(err) != "ResourceAlreadyExistsException" {
		t.Fatalf("duplicate bus err = %v", err)
	}
	described, err := client.DescribeEventBus(ctx, &eventbridge.DescribeEventBusInput{Name: bus.EventBusArn})
	if err != nil || aws.ToString(described.Name) != "orders" || aws.ToString(described.Description) != "order events" {
		t.Fatalf("described = %+v %v", described, err)
	}
	buses, err := client.ListEventBuses(ctx, &eventbridge.ListEventBusesInput{})
	if err != nil || len(buses.EventBuses) != 2 {
		t.Fatalf("buses = %+v %v", buses, err)
	}
	if _, err := client.PutPermission(ctx, &eventbridge.PutPermissionInput{EventBusName: aws.String("orders"), StatementId: aws.String("other"), Action: aws.String("events:PutEvents"), Principal: aws.String("111111111111")}); err != nil {
		t.Fatal(err)
	}
	described, err = client.DescribeEventBus(ctx, &eventbridge.DescribeEventBusInput{Name: aws.String("orders")})
	if err != nil || !strings.Contains(aws.ToString(described.Policy), "111111111111") {
		t.Fatalf("policy = %+v %v", described, err)
	}

	url, arn := queueWithArn(t, queues, "replayed", nil)
	if _, err := client.PutRule(ctx, &eventbridge.PutRuleInput{Name: aws.String("all"), EventBusName: aws.String("orders"), EventPattern: aws.String(`{"source": [{"prefix": ""}]}`)}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.PutTargets(ctx, &eventbridge.PutTargetsInput{Rule: aws.String("all"), EventBusName: aws.String("orders"), Targets: []types.Target{{Id: aws.String("q"), Arn: aws.String(arn)}}}); err != nil {
		t.Fatal(err)
	}
	archive, err := client.CreateArchive(ctx, &eventbridge.CreateArchiveInput{ArchiveName: aws.String("keep"), EventSourceArn: bus.EventBusArn, EventPattern: aws.String(`{"detail-type": ["kept"]}`)})
	if err != nil {
		t.Fatal(err)
	}
	start := time.Now().Add(-time.Minute)
	for _, dt := range []string{"kept", "dropped", "kept"} {
		if _, err := client.PutEvents(ctx, &eventbridge.PutEventsInput{Entries: []types.PutEventsRequestEntry{{Source: aws.String("shop"), DetailType: aws.String(dt), Detail: aws.String("{}"), EventBusName: aws.String("orders")}}}); err != nil {
			t.Fatal(err)
		}
	}
	if got := drain(t, queues, url); len(got) != 3 {
		t.Fatalf("live delivery = %d", len(got))
	}
	desc, err := client.DescribeArchive(ctx, &eventbridge.DescribeArchiveInput{ArchiveName: aws.String("keep")})
	if err != nil || desc.EventCount != 2 || desc.State != types.ArchiveStateEnabled {
		t.Fatalf("archive = %+v %v", desc, err)
	}

	replay, err := client.StartReplay(ctx, &eventbridge.StartReplayInput{
		ReplayName:     aws.String("again"),
		EventSourceArn: archive.ArchiveArn,
		EventStartTime: aws.Time(start),
		EventEndTime:   aws.Time(time.Now().Add(time.Minute)),
		Destination:    &types.ReplayDestination{Arn: bus.EventBusArn},
	})
	if err != nil {
		t.Fatal(err)
	}
	if replay.State != types.ReplayStateStarting {
		t.Fatalf("replay = %+v", replay)
	}
	replayed := drain(t, queues, url)
	if len(replayed) != 2 || !strings.Contains(aws.ToString(replayed[0].Body), `"replay-name":"again"`) {
		t.Fatalf("replayed = %+v", replayed)
	}
	status, err := client.DescribeReplay(ctx, &eventbridge.DescribeReplayInput{ReplayName: aws.String("again")})
	if err != nil || status.State != types.ReplayStateCompleted {
		t.Fatalf("replay status = %+v %v", status, err)
	}
	if _, err := client.CancelReplay(ctx, &eventbridge.CancelReplayInput{ReplayName: aws.String("again")}); apiCode(err) != "IllegalStatusException" {
		t.Fatalf("cancel completed replay err = %v", err)
	}

	if _, err := client.TagResource(ctx, &eventbridge.TagResourceInput{ResourceARN: bus.EventBusArn, Tags: []types.Tag{{Key: aws.String("team"), Value: aws.String("shop")}}}); err != nil {
		t.Fatal(err)
	}
	tags, err := client.ListTagsForResource(ctx, &eventbridge.ListTagsForResourceInput{ResourceARN: bus.EventBusArn})
	if err != nil || len(tags.Tags) != 1 {
		t.Fatalf("tags = %+v %v", tags, err)
	}

	conn, err := client.CreateConnection(ctx, &eventbridge.CreateConnectionInput{
		Name:              aws.String("api"),
		AuthorizationType: types.ConnectionAuthorizationTypeApiKey,
		AuthParameters:    &types.CreateConnectionAuthRequestParameters{ApiKeyAuthParameters: &types.CreateConnectionApiKeyAuthRequestParameters{ApiKeyName: aws.String("x-key"), ApiKeyValue: aws.String("v")}},
	})
	if err != nil || conn.ConnectionState != types.ConnectionStateAuthorized {
		t.Fatalf("connection = %+v %v", conn, err)
	}
	dest, err := client.CreateApiDestination(ctx, &eventbridge.CreateApiDestinationInput{Name: aws.String("hook"), ConnectionArn: conn.ConnectionArn, InvocationEndpoint: aws.String("https://example.com"), HttpMethod: types.ApiDestinationHttpMethodPost})
	if err != nil || dest.ApiDestinationState != types.ApiDestinationStateActive {
		t.Fatalf("api destination = %+v %v", dest, err)
	}
	if _, err := client.DeauthorizeConnection(ctx, &eventbridge.DeauthorizeConnectionInput{Name: aws.String("api")}); err != nil {
		t.Fatal(err)
	}
	dc, err := client.DescribeConnection(ctx, &eventbridge.DescribeConnectionInput{Name: aws.String("api")})
	if err != nil || dc.ConnectionState != types.ConnectionStateDeauthorized || dc.AuthorizationType != types.ConnectionAuthorizationTypeApiKey {
		t.Fatalf("described connection = %+v %v", dc, err)
	}

	if _, err := client.DeleteEventBus(ctx, &eventbridge.DeleteEventBusInput{Name: aws.String("default")}); apiCode(err) != "ValidationException" {
		t.Fatalf("delete default bus err = %v", err)
	}
	if _, err := client.DeleteEventBus(ctx, &eventbridge.DeleteEventBusInput{Name: aws.String("orders")}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.DescribeEventBus(ctx, &eventbridge.DescribeEventBusInput{Name: aws.String("orders")}); apiCode(err) != "ResourceNotFoundException" {
		t.Fatalf("deleted bus err = %v", err)
	}
}

func TestEventBridgeMocksAreIsolated(t *testing.T) {
	a := eventbridge.NewFromConfig(newMock(t).Config())
	b := eventbridge.NewFromConfig(newMock(t).Config())
	putRule(t, a, "only-in-a", `{"source": ["x"]}`)
	out, err := b.ListRules(context.Background(), &eventbridge.ListRulesInput{})
	if err != nil {
		t.Fatal(err)
	}
	if len(out.Rules) != 0 {
		t.Fatalf("mock b sees %+v", out.Rules)
	}
}

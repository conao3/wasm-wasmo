package wasmo_test

import (
	"context"
	"encoding/json"
	"strconv"
	"strings"
	"testing"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/sns"
	"github.com/aws/aws-sdk-go-v2/service/sns/types"
	"github.com/aws/aws-sdk-go-v2/service/sqs"
)

type envelope struct {
	Type              string
	MessageId         string
	TopicArn          string
	Subject           string
	Message           string
	SequenceNumber    string
	MessageAttributes map[string]struct{ Type, Value string }
}

func decodeEnvelope(t *testing.T, body string) envelope {
	t.Helper()
	var e envelope
	if err := json.Unmarshal([]byte(body), &e); err != nil {
		t.Fatalf("body %q is not an SNS envelope: %v", body, err)
	}
	return e
}

func mustTopic(t *testing.T, client *sns.Client, name string, attrs map[string]string) string {
	t.Helper()
	out, err := client.CreateTopic(context.Background(), &sns.CreateTopicInput{Name: aws.String(name), Attributes: attrs})
	if err != nil {
		t.Fatal(err)
	}
	return aws.ToString(out.TopicArn)
}

func subscribeQueue(t *testing.T, client *sns.Client, topic, queueArn string, attrs map[string]string) string {
	t.Helper()
	out, err := client.Subscribe(context.Background(), &sns.SubscribeInput{
		TopicArn:   aws.String(topic),
		Protocol:   aws.String("sqs"),
		Endpoint:   aws.String(queueArn),
		Attributes: attrs,
	})
	if err != nil {
		t.Fatal(err)
	}
	return aws.ToString(out.SubscriptionArn)
}

func TestSNSTopics(t *testing.T) {
	ctx := context.Background()
	client := sns.NewFromConfig(newMock(t).Config())

	arn := mustTopic(t, client, "orders", map[string]string{"DisplayName": "Orders"})
	if arn != "arn:aws:sns:us-east-1:000000000000:orders" {
		t.Fatalf("arn = %s", arn)
	}
	if again := mustTopic(t, client, "orders", nil); again != arn {
		t.Fatalf("CreateTopic is not idempotent: %s", again)
	}
	if _, err := client.CreateTopic(ctx, &sns.CreateTopicInput{Name: aws.String("orders"), Attributes: map[string]string{"DisplayName": "Other"}}); apiCode(err) != "InvalidParameter" {
		t.Fatalf("conflicting CreateTopic err = %v", err)
	}
	if _, err := client.CreateTopic(ctx, &sns.CreateTopicInput{Name: aws.String("bad name")}); apiCode(err) != "InvalidParameter" {
		t.Fatalf("invalid name err = %v", err)
	}

	attrs, err := client.GetTopicAttributes(ctx, &sns.GetTopicAttributesInput{TopicArn: aws.String(arn)})
	if err != nil {
		t.Fatal(err)
	}
	if attrs.Attributes["DisplayName"] != "Orders" || attrs.Attributes["Owner"] != "000000000000" || !strings.Contains(attrs.Attributes["Policy"], "SNS:Publish") {
		t.Fatalf("attributes = %v", attrs.Attributes)
	}
	if _, err := client.SetTopicAttributes(ctx, &sns.SetTopicAttributesInput{TopicArn: aws.String(arn), AttributeName: aws.String("DisplayName"), AttributeValue: aws.String("Renamed")}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.SetTopicAttributes(ctx, &sns.SetTopicAttributesInput{TopicArn: aws.String(arn), AttributeName: aws.String("Bogus"), AttributeValue: aws.String("x")}); apiCode(err) != "InvalidParameter" {
		t.Fatalf("bogus attribute err = %v", err)
	}

	if _, err := client.TagResource(ctx, &sns.TagResourceInput{ResourceArn: aws.String(arn), Tags: []types.Tag{{Key: aws.String("env"), Value: aws.String("test")}, {Key: aws.String("team"), Value: aws.String("a")}}}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.UntagResource(ctx, &sns.UntagResourceInput{ResourceArn: aws.String(arn), TagKeys: []string{"team"}}); err != nil {
		t.Fatal(err)
	}
	tags, err := client.ListTagsForResource(ctx, &sns.ListTagsForResourceInput{ResourceArn: aws.String(arn)})
	if err != nil {
		t.Fatal(err)
	}
	if len(tags.Tags) != 1 || aws.ToString(tags.Tags[0].Key) != "env" {
		t.Fatalf("tags = %+v", tags.Tags)
	}

	for i := range 105 {
		mustTopic(t, client, "t"+strconv.Itoa(i), nil)
	}
	var listed int
	pager := sns.NewListTopicsPaginator(client, &sns.ListTopicsInput{})
	for pager.HasMorePages() {
		page, err := pager.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		listed += len(page.Topics)
	}
	if listed != 106 {
		t.Fatalf("listed %d topics", listed)
	}

	if _, err := client.DeleteTopic(ctx, &sns.DeleteTopicInput{TopicArn: aws.String(arn)}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.GetTopicAttributes(ctx, &sns.GetTopicAttributesInput{TopicArn: aws.String(arn)}); apiCode(err) != "NotFound" {
		t.Fatalf("deleted topic err = %v", err)
	}
	if _, err := client.Publish(ctx, &sns.PublishInput{TopicArn: aws.String(arn), Message: aws.String("x")}); apiCode(err) != "NotFound" {
		t.Fatalf("publish to deleted topic err = %v", err)
	}
}

func TestSNSToSQS(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	client := sns.NewFromConfig(m.Config())
	queues := sqs.NewFromConfig(m.Config())

	topic := mustTopic(t, client, "events", nil)
	envURL, envArn := queueWithArn(t, queues, "enveloped", nil)
	rawURL, rawArn := queueWithArn(t, queues, "raw", nil)
	subscribeQueue(t, client, topic, envArn, nil)
	rawSub := subscribeQueue(t, client, topic, rawArn, map[string]string{"RawMessageDelivery": "true"})
	if !strings.HasPrefix(rawSub, topic+":") {
		t.Fatalf("subscription arn = %s", rawSub)
	}

	published, err := client.Publish(ctx, &sns.PublishInput{
		TopicArn: aws.String(topic),
		Message:  aws.String("hello"),
		Subject:  aws.String("greeting"),
		MessageAttributes: map[string]types.MessageAttributeValue{
			"kind": {DataType: aws.String("String"), StringValue: aws.String("welcome")},
		},
	})
	if err != nil {
		t.Fatal(err)
	}

	got := drain(t, queues, envURL)
	if len(got) != 1 {
		t.Fatalf("enveloped queue got %d messages", len(got))
	}
	e := decodeEnvelope(t, aws.ToString(got[0].Body))
	if e.Type != "Notification" || e.Message != "hello" || e.Subject != "greeting" || e.TopicArn != topic || e.MessageId != aws.ToString(published.MessageId) {
		t.Fatalf("envelope = %+v", e)
	}
	if e.MessageAttributes["kind"].Value != "welcome" || e.MessageAttributes["kind"].Type != "String" {
		t.Fatalf("envelope attributes = %+v", e.MessageAttributes)
	}

	raw := drain(t, queues, rawURL)
	if len(raw) != 1 || aws.ToString(raw[0].Body) != "hello" {
		t.Fatalf("raw queue = %+v", raw)
	}
	if aws.ToString(raw[0].MessageAttributes["kind"].StringValue) != "welcome" {
		t.Fatalf("raw attributes = %+v", raw[0].MessageAttributes)
	}

	structured, err := json.Marshal(map[string]string{"default": "plain", "sqs": "for queues"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := client.Publish(ctx, &sns.PublishInput{TopicArn: aws.String(topic), Message: aws.String(string(structured)), MessageStructure: aws.String("json")}); err != nil {
		t.Fatal(err)
	}
	if got := drain(t, queues, rawURL); len(got) != 1 || aws.ToString(got[0].Body) != "for queues" {
		t.Fatalf("json structure delivery = %+v", got)
	}
	drain(t, queues, envURL)

	if _, err := client.Unsubscribe(ctx, &sns.UnsubscribeInput{SubscriptionArn: aws.String(rawSub)}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.Publish(ctx, &sns.PublishInput{TopicArn: aws.String(topic), Message: aws.String("after")}); err != nil {
		t.Fatal(err)
	}
	if got := drain(t, queues, rawURL); len(got) != 0 {
		t.Fatalf("unsubscribed queue received %+v", got)
	}
	if got := drain(t, queues, envURL); len(got) != 1 {
		t.Fatalf("subscribed queue received %d", len(got))
	}
}

func TestSNSFilterPolicy(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	client := sns.NewFromConfig(m.Config())
	queues := sqs.NewFromConfig(m.Config())

	topic := mustTopic(t, client, "orders", nil)
	bigURL, bigArn := queueWithArn(t, queues, "big-red", nil)
	bodyURL, bodyArn := queueWithArn(t, queues, "by-body", nil)
	subscribeQueue(t, client, topic, bigArn, map[string]string{
		"RawMessageDelivery": "true",
		"FilterPolicy":       `{"color": ["red"], "amount": [{"numeric": [">=", 100]}], "region": [{"anything-but": "eu"}]}`,
	})
	bodySub := subscribeQueue(t, client, topic, bodyArn, map[string]string{"RawMessageDelivery": "true"})
	if _, err := client.SetSubscriptionAttributes(ctx, &sns.SetSubscriptionAttributesInput{
		SubscriptionArn: aws.String(bodySub),
		AttributeName:   aws.String("FilterPolicyScope"),
		AttributeValue:  aws.String("MessageBody"),
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.SetSubscriptionAttributes(ctx, &sns.SetSubscriptionAttributesInput{
		SubscriptionArn: aws.String(bodySub),
		AttributeName:   aws.String("FilterPolicy"),
		AttributeValue:  aws.String(`{"order": {"sku": [{"prefix": "BOOK-"}]}}`),
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.SetSubscriptionAttributes(ctx, &sns.SetSubscriptionAttributesInput{
		SubscriptionArn: aws.String(bodySub),
		AttributeName:   aws.String("FilterPolicy"),
		AttributeValue:  aws.String(`{"order": [{"bogus": 1}]}`),
	}); apiCode(err) != "InvalidParameter" {
		t.Fatalf("invalid filter policy err = %v", err)
	}
	attrs, err := client.GetSubscriptionAttributes(ctx, &sns.GetSubscriptionAttributesInput{SubscriptionArn: aws.String(bodySub)})
	if err != nil {
		t.Fatal(err)
	}
	if attrs.Attributes["FilterPolicyScope"] != "MessageBody" || attrs.Attributes["PendingConfirmation"] != "false" {
		t.Fatalf("subscription attributes = %v", attrs.Attributes)
	}

	publish := func(body, color, amount, region string) {
		t.Helper()
		in := &sns.PublishInput{TopicArn: aws.String(topic), Message: aws.String(body), MessageAttributes: map[string]types.MessageAttributeValue{}}
		if color != "" {
			in.MessageAttributes["color"] = types.MessageAttributeValue{DataType: aws.String("String"), StringValue: aws.String(color)}
		}
		if amount != "" {
			in.MessageAttributes["amount"] = types.MessageAttributeValue{DataType: aws.String("Number"), StringValue: aws.String(amount)}
		}
		if region != "" {
			in.MessageAttributes["region"] = types.MessageAttributeValue{DataType: aws.String("String"), StringValue: aws.String(region)}
		}
		if _, err := client.Publish(ctx, in); err != nil {
			t.Fatal(err)
		}
	}
	publish("match", "red", "150", "us")
	publish("small", "red", "99", "us")
	publish("blue", "blue", "150", "us")
	publish("eu", "red", "150", "eu")
	publish("no-amount", "red", "", "us")
	publish(`{"order": {"sku": "BOOK-1"}}`, "", "", "")
	publish(`{"order": {"sku": "TOY-1"}}`, "", "", "")

	if got := drain(t, queues, bigURL); len(got) != 1 || aws.ToString(got[0].Body) != "match" {
		t.Fatalf("attribute filter delivered %+v", got)
	}
	if got := drain(t, queues, bodyURL); len(got) != 1 || !strings.Contains(aws.ToString(got[0].Body), "BOOK-1") {
		t.Fatalf("body filter delivered %+v", got)
	}
}

func TestSNSFifoAndBatch(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	client := sns.NewFromConfig(m.Config())
	queues := sqs.NewFromConfig(m.Config())

	if _, err := client.CreateTopic(ctx, &sns.CreateTopicInput{Name: aws.String("plain"), Attributes: map[string]string{"FifoTopic": "true"}}); apiCode(err) != "InvalidParameter" {
		t.Fatalf("fifo topic without suffix err = %v", err)
	}
	topic := mustTopic(t, client, "orders.fifo", map[string]string{"FifoTopic": "true", "ContentBasedDeduplication": "true"})
	url, arn := queueWithArn(t, queues, "orders.fifo", map[string]string{"FifoQueue": "true"})
	_, plainArn := queueWithArn(t, queues, "plain", nil)
	if _, err := client.Subscribe(ctx, &sns.SubscribeInput{TopicArn: aws.String(topic), Protocol: aws.String("sqs"), Endpoint: aws.String(plainArn)}); apiCode(err) != "InvalidParameter" {
		t.Fatalf("standard queue on fifo topic err = %v", err)
	}
	subscribeQueue(t, client, topic, arn, map[string]string{"RawMessageDelivery": "true"})

	if _, err := client.Publish(ctx, &sns.PublishInput{TopicArn: aws.String(topic), Message: aws.String("x")}); apiCode(err) != "InvalidParameter" {
		t.Fatalf("missing group id err = %v", err)
	}
	first, err := client.Publish(ctx, &sns.PublishInput{TopicArn: aws.String(topic), Message: aws.String("one"), MessageGroupId: aws.String("g")})
	if err != nil {
		t.Fatal(err)
	}
	dup, err := client.Publish(ctx, &sns.PublishInput{TopicArn: aws.String(topic), Message: aws.String("one"), MessageGroupId: aws.String("g")})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(dup.MessageId) != aws.ToString(first.MessageId) || aws.ToString(first.SequenceNumber) == "" {
		t.Fatalf("dedup: first=%+v dup=%+v", first, dup)
	}

	batch, err := client.PublishBatch(ctx, &sns.PublishBatchInput{
		TopicArn: aws.String(topic),
		PublishBatchRequestEntries: []types.PublishBatchRequestEntry{
			{Id: aws.String("a"), Message: aws.String("two"), MessageGroupId: aws.String("g")},
			{Id: aws.String("b"), Message: aws.String("three"), MessageGroupId: aws.String("g")},
			{Id: aws.String("c"), Message: aws.String("four")},
		},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(batch.Successful) != 2 || len(batch.Failed) != 1 || aws.ToString(batch.Failed[0].Id) != "c" {
		t.Fatalf("batch = %+v", batch)
	}
	if _, err := client.PublishBatch(ctx, &sns.PublishBatchInput{
		TopicArn: aws.String(topic),
		PublishBatchRequestEntries: []types.PublishBatchRequestEntry{
			{Id: aws.String("a"), Message: aws.String("x")},
			{Id: aws.String("a"), Message: aws.String("y")},
		},
	}); apiCode(err) != "BatchEntryIdsNotDistinct" {
		t.Fatalf("duplicate ids err = %v", err)
	}

	var bodies []string
	for {
		out, err := queues.ReceiveMessage(ctx, &sqs.ReceiveMessageInput{QueueUrl: aws.String(url), MaxNumberOfMessages: 10})
		if err != nil {
			t.Fatal(err)
		}
		if len(out.Messages) == 0 {
			break
		}
		for _, msg := range out.Messages {
			bodies = append(bodies, aws.ToString(msg.Body))
			if _, err := queues.DeleteMessage(ctx, &sqs.DeleteMessageInput{QueueUrl: aws.String(url), ReceiptHandle: msg.ReceiptHandle}); err != nil {
				t.Fatal(err)
			}
		}
	}
	if strings.Join(bodies, ",") != "one,two,three" {
		t.Fatalf("fifo queue bodies = %v", bodies)
	}
}

func TestSNSConfirmation(t *testing.T) {
	ctx := context.Background()
	client := sns.NewFromConfig(newMock(t).Config())
	topic := mustTopic(t, client, "hooks", nil)

	if _, err := client.Subscribe(ctx, &sns.SubscribeInput{TopicArn: aws.String(topic), Protocol: aws.String("carrier-pigeon"), Endpoint: aws.String("x")}); apiCode(err) != "InvalidParameter" {
		t.Fatalf("unknown protocol err = %v", err)
	}
	pending, err := client.Subscribe(ctx, &sns.SubscribeInput{TopicArn: aws.String(topic), Protocol: aws.String("https"), Endpoint: aws.String("https://example.com/hook")})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(pending.SubscriptionArn) != "pending confirmation" {
		t.Fatalf("subscription arn = %s", aws.ToString(pending.SubscriptionArn))
	}
	subs, err := client.ListSubscriptionsByTopic(ctx, &sns.ListSubscriptionsByTopicInput{TopicArn: aws.String(topic)})
	if err != nil {
		t.Fatal(err)
	}
	if len(subs.Subscriptions) != 1 || aws.ToString(subs.Subscriptions[0].SubscriptionArn) != "PendingConfirmation" {
		t.Fatalf("subscriptions = %+v", subs.Subscriptions)
	}
	withArn, err := client.Subscribe(ctx, &sns.SubscribeInput{TopicArn: aws.String(topic), Protocol: aws.String("https"), Endpoint: aws.String("https://example.com/hook"), ReturnSubscriptionArn: true})
	if err != nil {
		t.Fatal(err)
	}
	attrs, err := client.GetSubscriptionAttributes(ctx, &sns.GetSubscriptionAttributesInput{SubscriptionArn: withArn.SubscriptionArn})
	if err != nil {
		t.Fatal(err)
	}
	if attrs.Attributes["PendingConfirmation"] != "true" || attrs.Attributes["PendingToken"] == "" {
		t.Fatalf("pending attributes = %v", attrs.Attributes)
	}
	topicAttrs, err := client.GetTopicAttributes(ctx, &sns.GetTopicAttributesInput{TopicArn: aws.String(topic)})
	if err != nil {
		t.Fatal(err)
	}
	if topicAttrs.Attributes["SubscriptionsPending"] != "1" {
		t.Fatalf("topic attributes = %v", topicAttrs.Attributes)
	}

	if _, err := client.ConfirmSubscription(ctx, &sns.ConfirmSubscriptionInput{TopicArn: aws.String(topic), Token: aws.String("wrong")}); apiCode(err) != "InvalidParameter" {
		t.Fatalf("bad token err = %v", err)
	}
	confirmed, err := client.ConfirmSubscription(ctx, &sns.ConfirmSubscriptionInput{TopicArn: aws.String(topic), Token: aws.String(attrs.Attributes["PendingToken"])})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(confirmed.SubscriptionArn) != aws.ToString(withArn.SubscriptionArn) {
		t.Fatalf("confirmed = %s", aws.ToString(confirmed.SubscriptionArn))
	}
	topicAttrs, err = client.GetTopicAttributes(ctx, &sns.GetTopicAttributesInput{TopicArn: aws.String(topic)})
	if err != nil {
		t.Fatal(err)
	}
	if topicAttrs.Attributes["SubscriptionsConfirmed"] != "1" || topicAttrs.Attributes["SubscriptionsPending"] != "0" {
		t.Fatalf("topic attributes after confirm = %v", topicAttrs.Attributes)
	}
}

func TestSNSPlatformAndSMS(t *testing.T) {
	ctx := context.Background()
	client := sns.NewFromConfig(newMock(t).Config())

	app, err := client.CreatePlatformApplication(ctx, &sns.CreatePlatformApplicationInput{Name: aws.String("app"), Platform: aws.String("GCM"), Attributes: map[string]string{"PlatformCredential": "key"}})
	if err != nil {
		t.Fatal(err)
	}
	ep, err := client.CreatePlatformEndpoint(ctx, &sns.CreatePlatformEndpointInput{PlatformApplicationArn: app.PlatformApplicationArn, Token: aws.String("device-1")})
	if err != nil {
		t.Fatal(err)
	}
	same, err := client.CreatePlatformEndpoint(ctx, &sns.CreatePlatformEndpointInput{PlatformApplicationArn: app.PlatformApplicationArn, Token: aws.String("device-1")})
	if err != nil || aws.ToString(same.EndpointArn) != aws.ToString(ep.EndpointArn) {
		t.Fatalf("endpoint not idempotent: %v %v", same, err)
	}
	if _, err := client.Publish(ctx, &sns.PublishInput{TargetArn: ep.EndpointArn, Message: aws.String("push")}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.SetEndpointAttributes(ctx, &sns.SetEndpointAttributesInput{EndpointArn: ep.EndpointArn, Attributes: map[string]string{"Enabled": "false"}}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.Publish(ctx, &sns.PublishInput{TargetArn: ep.EndpointArn, Message: aws.String("push")}); apiCode(err) != "EndpointDisabled" {
		t.Fatalf("disabled endpoint err = %v", err)
	}
	eps, err := client.ListEndpointsByPlatformApplication(ctx, &sns.ListEndpointsByPlatformApplicationInput{PlatformApplicationArn: app.PlatformApplicationArn})
	if err != nil || len(eps.Endpoints) != 1 {
		t.Fatalf("endpoints = %+v %v", eps, err)
	}
	if _, err := client.DeletePlatformApplication(ctx, &sns.DeletePlatformApplicationInput{PlatformApplicationArn: app.PlatformApplicationArn}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.GetEndpointAttributes(ctx, &sns.GetEndpointAttributesInput{EndpointArn: ep.EndpointArn}); apiCode(err) != "NotFound" {
		t.Fatalf("endpoint of deleted app err = %v", err)
	}

	if _, err := client.SetSMSAttributes(ctx, &sns.SetSMSAttributesInput{Attributes: map[string]string{"DefaultSMSType": "Transactional"}}); err != nil {
		t.Fatal(err)
	}
	sms, err := client.GetSMSAttributes(ctx, &sns.GetSMSAttributesInput{})
	if err != nil || sms.Attributes["DefaultSMSType"] != "Transactional" {
		t.Fatalf("sms attributes = %+v %v", sms, err)
	}
	if _, err := client.Publish(ctx, &sns.PublishInput{PhoneNumber: aws.String("+15555550100"), Message: aws.String("code 1234")}); err != nil {
		t.Fatal(err)
	}
}

func TestSNSMocksAreIsolated(t *testing.T) {
	a := sns.NewFromConfig(newMock(t).Config())
	b := sns.NewFromConfig(newMock(t).Config())
	mustTopic(t, a, "only-in-a", nil)
	out, err := b.ListTopics(context.Background(), &sns.ListTopicsInput{})
	if err != nil {
		t.Fatal(err)
	}
	if len(out.Topics) != 0 {
		t.Fatalf("mock b sees %+v", out.Topics)
	}
}

func TestSNSRedrivesUndeliverableMessages(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	client := sns.NewFromConfig(m.Config())
	queues := sqs.NewFromConfig(m.Config())

	topic := mustTopic(t, client, "jobs", nil)
	dlqURL, dlqArn := queueWithArn(t, queues, "jobs-dlq", nil)
	redrive := `{"deadLetterTargetArn": "` + dlqArn + `"}`
	if _, err := client.Subscribe(ctx, &sns.SubscribeInput{
		TopicArn:   aws.String(topic),
		Protocol:   aws.String("lambda"),
		Endpoint:   aws.String("arn:aws:lambda:us-east-1:000000000000:function:worker"),
		Attributes: map[string]string{"RedrivePolicy": redrive},
	}); err != nil {
		t.Fatal(err)
	}
	_, goneArn := queueWithArn(t, queues, "gone", nil)
	subscribeQueue(t, client, topic, goneArn, map[string]string{"RedrivePolicy": redrive, "RawMessageDelivery": "true"})
	goneURL, err := queues.GetQueueUrl(ctx, &sqs.GetQueueUrlInput{QueueName: aws.String("gone")})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := queues.DeleteQueue(ctx, &sqs.DeleteQueueInput{QueueUrl: goneURL.QueueUrl}); err != nil {
		t.Fatal(err)
	}

	if _, err := client.Publish(ctx, &sns.PublishInput{TopicArn: aws.String(topic), Message: aws.String("work")}); err != nil {
		t.Fatal(err)
	}
	got := drain(t, queues, dlqURL)
	if len(got) != 2 {
		t.Fatalf("dead-letter queue got %d messages", len(got))
	}
	var lambda struct {
		Records []struct {
			EventSource string
			Sns         struct{ Message string }
		}
	}
	if err := json.Unmarshal([]byte(aws.ToString(got[0].Body)), &lambda); err != nil || lambda.Records[0].EventSource != "aws:sns" || lambda.Records[0].Sns.Message != "work" {
		t.Fatalf("lambda payload = %s (%v)", aws.ToString(got[0].Body), err)
	}
	if aws.ToString(got[1].Body) != "work" {
		t.Fatalf("raw payload = %s", aws.ToString(got[1].Body))
	}
}

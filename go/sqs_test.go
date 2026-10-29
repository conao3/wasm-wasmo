package wasmo_test

import (
	"context"
	"errors"
	"strconv"
	"strings"
	"testing"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/sqs"
	"github.com/aws/aws-sdk-go-v2/service/sqs/types"
)

func TestSQSSendReceiveDelete(t *testing.T) {
	ctx := context.Background()
	client := sqs.NewFromConfig(newMock(t).Config())

	created, err := client.CreateQueue(ctx, &sqs.CreateQueueInput{QueueName: aws.String("jobs")})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := client.SendMessage(ctx, &sqs.SendMessageInput{
		QueueUrl:    created.QueueUrl,
		MessageBody: aws.String("hello 日本語"),
		MessageAttributes: map[string]types.MessageAttributeValue{
			"kind":  {DataType: aws.String("String"), StringValue: aws.String("greeting")},
			"bytes": {DataType: aws.String("Binary"), BinaryValue: []byte{0, 1, 2, 0xff}},
		},
	}); err != nil {
		t.Fatal(err)
	}

	got, err := client.ReceiveMessage(ctx, &sqs.ReceiveMessageInput{
		QueueUrl:                    created.QueueUrl,
		MessageAttributeNames:       []string{"All"},
		MessageSystemAttributeNames: []types.MessageSystemAttributeName{types.MessageSystemAttributeNameAll},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(got.Messages) != 1 || aws.ToString(got.Messages[0].Body) != "hello 日本語" {
		t.Fatalf("messages = %+v", got.Messages)
	}
	m := got.Messages[0]
	if b := m.MessageAttributes["bytes"].BinaryValue; len(b) != 4 || b[3] != 0xff {
		t.Fatalf("binary attribute = %v", b)
	}
	if m.Attributes["ApproximateReceiveCount"] != "1" {
		t.Fatalf("attributes = %v", m.Attributes)
	}

	again, err := client.ReceiveMessage(ctx, &sqs.ReceiveMessageInput{QueueUrl: created.QueueUrl})
	if err != nil {
		t.Fatal(err)
	}
	if len(again.Messages) != 0 {
		t.Fatalf("in-flight message was received twice: %+v", again.Messages)
	}

	if _, err := client.DeleteMessage(ctx, &sqs.DeleteMessageInput{QueueUrl: created.QueueUrl, ReceiptHandle: m.ReceiptHandle}); err != nil {
		t.Fatal(err)
	}
	attrs, err := client.GetQueueAttributes(ctx, &sqs.GetQueueAttributesInput{
		QueueUrl:       created.QueueUrl,
		AttributeNames: []types.QueueAttributeName{types.QueueAttributeNameAll},
	})
	if err != nil {
		t.Fatal(err)
	}
	if attrs.Attributes["ApproximateNumberOfMessages"] != "0" || attrs.Attributes["ApproximateNumberOfMessagesNotVisible"] != "0" {
		t.Fatalf("attributes after delete = %v", attrs.Attributes)
	}
}

func TestSQSVisibilityAndBatch(t *testing.T) {
	ctx := context.Background()
	client := sqs.NewFromConfig(newMock(t).Config())
	url := mustQueue(t, client, "batch", nil)

	entries := make([]types.SendMessageBatchRequestEntry, 3)
	for i := range entries {
		entries[i] = types.SendMessageBatchRequestEntry{Id: aws.String(strconv.Itoa(i)), MessageBody: aws.String("m" + strconv.Itoa(i))}
	}
	sent, err := client.SendMessageBatch(ctx, &sqs.SendMessageBatchInput{QueueUrl: url, Entries: entries})
	if err != nil {
		t.Fatal(err)
	}
	if len(sent.Successful) != 3 || len(sent.Failed) != 0 {
		t.Fatalf("batch = %+v", sent)
	}

	got, err := client.ReceiveMessage(ctx, &sqs.ReceiveMessageInput{QueueUrl: url, MaxNumberOfMessages: 10})
	if err != nil {
		t.Fatal(err)
	}
	if len(got.Messages) != 3 {
		t.Fatalf("received %d messages", len(got.Messages))
	}
	if _, err := client.ChangeMessageVisibility(ctx, &sqs.ChangeMessageVisibilityInput{
		QueueUrl: url, ReceiptHandle: got.Messages[0].ReceiptHandle, VisibilityTimeout: 0,
	}); err != nil {
		t.Fatal(err)
	}
	visible, err := client.ReceiveMessage(ctx, &sqs.ReceiveMessageInput{QueueUrl: url, MaxNumberOfMessages: 10})
	if err != nil {
		t.Fatal(err)
	}
	if len(visible.Messages) != 1 || aws.ToString(visible.Messages[0].Body) != aws.ToString(got.Messages[0].Body) {
		t.Fatalf("after visibility reset = %+v", visible.Messages)
	}
}

func TestSQSFifoDeduplication(t *testing.T) {
	ctx := context.Background()
	client := sqs.NewFromConfig(newMock(t).Config())
	url := mustQueue(t, client, "orders.fifo", map[string]string{"FifoQueue": "true", "ContentBasedDeduplication": "true"})

	var ids []string
	for range 2 {
		out, err := client.SendMessage(ctx, &sqs.SendMessageInput{QueueUrl: url, MessageBody: aws.String("same"), MessageGroupId: aws.String("g")})
		if err != nil {
			t.Fatal(err)
		}
		ids = append(ids, aws.ToString(out.MessageId))
	}
	if ids[0] != ids[1] {
		t.Fatalf("duplicate send returned a new message id: %v", ids)
	}
	got, err := client.ReceiveMessage(ctx, &sqs.ReceiveMessageInput{QueueUrl: url, MaxNumberOfMessages: 10})
	if err != nil {
		t.Fatal(err)
	}
	if len(got.Messages) != 1 {
		t.Fatalf("received %d messages", len(got.Messages))
	}
}

func TestSQSDeadLetterRedrive(t *testing.T) {
	ctx := context.Background()
	client := sqs.NewFromConfig(newMock(t).Config())
	dlq := mustQueue(t, client, "dlq", nil)
	dlqAttrs, err := client.GetQueueAttributes(ctx, &sqs.GetQueueAttributesInput{QueueUrl: dlq, AttributeNames: []types.QueueAttributeName{types.QueueAttributeNameQueueArn}})
	if err != nil {
		t.Fatal(err)
	}
	src := mustQueue(t, client, "src", map[string]string{
		"RedrivePolicy": `{"deadLetterTargetArn":"` + dlqAttrs.Attributes["QueueArn"] + `","maxReceiveCount":"1"}`,
	})
	if _, err := client.SendMessage(ctx, &sqs.SendMessageInput{QueueUrl: src, MessageBody: aws.String("poison")}); err != nil {
		t.Fatal(err)
	}
	first, err := client.ReceiveMessage(ctx, &sqs.ReceiveMessageInput{QueueUrl: src})
	if err != nil || len(first.Messages) != 1 {
		t.Fatalf("first receive = %+v, %v", first, err)
	}
	if _, err := client.ChangeMessageVisibility(ctx, &sqs.ChangeMessageVisibilityInput{QueueUrl: src, ReceiptHandle: first.Messages[0].ReceiptHandle}); err != nil {
		t.Fatal(err)
	}
	if again, _ := client.ReceiveMessage(ctx, &sqs.ReceiveMessageInput{QueueUrl: src}); len(again.Messages) != 0 {
		t.Fatalf("message was not redriven: %+v", again.Messages)
	}
	moved, err := client.ReceiveMessage(ctx, &sqs.ReceiveMessageInput{QueueUrl: dlq})
	if err != nil || len(moved.Messages) != 1 || aws.ToString(moved.Messages[0].Body) != "poison" {
		t.Fatalf("dlq receive = %+v, %v", moved, err)
	}
	sources, err := client.ListDeadLetterSourceQueues(ctx, &sqs.ListDeadLetterSourceQueuesInput{QueueUrl: dlq})
	if err != nil || len(sources.QueueUrls) != 1 {
		t.Fatalf("dead letter sources = %+v, %v", sources, err)
	}
}

func TestSQSErrorsAndIsolation(t *testing.T) {
	ctx := context.Background()
	a := sqs.NewFromConfig(newMock(t).Config())
	b := sqs.NewFromConfig(newMock(t).Config())
	mustQueue(t, a, "only-in-a", nil)

	_, err := b.GetQueueUrl(ctx, &sqs.GetQueueUrlInput{QueueName: aws.String("only-in-a")})
	var missing *types.QueueDoesNotExist
	if !errors.As(err, &missing) {
		t.Fatalf("expected QueueDoesNotExist from an isolated mock, got %v", err)
	}
	list, err := a.ListQueues(ctx, &sqs.ListQueuesInput{})
	if err != nil || len(list.QueueUrls) != 1 {
		t.Fatalf("list = %+v, %v", list, err)
	}
}

func mustQueue(t *testing.T, client *sqs.Client, name string, attrs map[string]string) *string {
	t.Helper()
	out, err := client.CreateQueue(context.Background(), &sqs.CreateQueueInput{QueueName: aws.String(name), Attributes: attrs})
	if err != nil {
		t.Fatal(err)
	}
	return out.QueueUrl
}

func TestSQSFifoGroupOrder(t *testing.T) {
	ctx := context.Background()
	client := sqs.NewFromConfig(newMock(t).Config())
	url := mustQueue(t, client, "steps.fifo", map[string]string{"FifoQueue": "true", "ContentBasedDeduplication": "true"})
	for _, body := range []string{"1", "2", "3"} {
		if _, err := client.SendMessage(ctx, &sqs.SendMessageInput{QueueUrl: url, MessageBody: aws.String(body), MessageGroupId: aws.String("g")}); err != nil {
			t.Fatal(err)
		}
	}
	got, err := client.ReceiveMessage(ctx, &sqs.ReceiveMessageInput{QueueUrl: url, MaxNumberOfMessages: 10})
	if err != nil {
		t.Fatal(err)
	}
	var bodies []string
	for _, m := range got.Messages {
		bodies = append(bodies, aws.ToString(m.Body))
	}
	if strings.Join(bodies, ",") != "1,2,3" {
		t.Fatalf("one receive returned %v, want the whole group in order", bodies)
	}
	again, err := client.ReceiveMessage(ctx, &sqs.ReceiveMessageInput{QueueUrl: url, MaxNumberOfMessages: 10})
	if err != nil || len(again.Messages) != 0 {
		t.Fatalf("group with in-flight messages was delivered again: %+v, %v", again, err)
	}
}

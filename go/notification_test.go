package wasmo_test

import (
	"context"
	"encoding/json"
	"testing"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/eventbridge"
	ebtypes "github.com/aws/aws-sdk-go-v2/service/eventbridge/types"
	"github.com/aws/aws-sdk-go-v2/service/s3"
	"github.com/aws/aws-sdk-go-v2/service/s3/types"
	"github.com/aws/aws-sdk-go-v2/service/sns"
	"github.com/aws/aws-sdk-go-v2/service/sqs"
	sqstypes "github.com/aws/aws-sdk-go-v2/service/sqs/types"
	wasmo "github.com/conao3/wasm-wasmo/go"
)

type s3Event struct {
	Records []struct {
		EventSource string `json:"eventSource"`
		EventName   string `json:"eventName"`
		AwsRegion   string `json:"awsRegion"`
		S3          struct {
			ConfigurationID string `json:"configurationId"`
			Bucket          struct {
				Name string `json:"name"`
				Arn  string `json:"arn"`
			} `json:"bucket"`
			Object struct {
				Key  string `json:"key"`
				Size int    `json:"size"`
				ETag string `json:"eTag"`
			} `json:"object"`
		} `json:"s3"`
	}
	Event string
}

func s3Events(t *testing.T, msgs []sqstypes.Message) []s3Event {
	t.Helper()
	var out []s3Event
	for _, msg := range msgs {
		var e s3Event
		if err := json.Unmarshal([]byte(aws.ToString(msg.Body)), &e); err != nil {
			t.Fatalf("body %q: %v", aws.ToString(msg.Body), err)
		}
		out = append(out, e)
	}
	return out
}

func TestS3NotificationToSQS(t *testing.T) {
	eachAddressing(t, func(t *testing.T, m *wasmo.Mock, c *s3.Client) {
		ctx := context.Background()
		queues := sqs.NewFromConfig(m.Config())
		imagesURL, imagesArn := queueWithArn(t, queues, "images", nil)
		removedURL, removedArn := queueWithArn(t, queues, "removed", nil)
		mustBucket(t, c, "uploads")

		if _, err := c.PutBucketNotificationConfiguration(ctx, &s3.PutBucketNotificationConfigurationInput{
			Bucket: aws.String("uploads"),
			NotificationConfiguration: &types.NotificationConfiguration{QueueConfigurations: []types.QueueConfiguration{
				{
					Id:       aws.String("pngs"),
					QueueArn: aws.String(imagesArn),
					Events:   []types.Event{types.EventS3ObjectCreated},
					Filter: &types.NotificationConfigurationFilter{Key: &types.S3KeyFilter{FilterRules: []types.FilterRule{
						{Name: types.FilterRuleNamePrefix, Value: aws.String("img/")},
						{Name: types.FilterRuleNameSuffix, Value: aws.String(".png")},
					}}},
				},
				{Id: aws.String("deletes"), QueueArn: aws.String(removedArn), Events: []types.Event{"s3:ObjectRemoved:*"}},
			}},
		}); err != nil {
			t.Fatal(err)
		}
		got, err := c.GetBucketNotificationConfiguration(ctx, &s3.GetBucketNotificationConfigurationInput{Bucket: aws.String("uploads")})
		if err != nil || len(got.QueueConfigurations) != 2 {
			t.Fatalf("configuration = %+v %v", got, err)
		}
		for _, url := range []string{imagesURL, removedURL} {
			if e := s3Events(t, drain(t, queues, url)); len(e) != 1 || e[0].Event != "s3:TestEvent" {
				t.Fatalf("test event = %+v", e)
			}
		}

		put := putString(t, c, "uploads", "img/cat.png", "meow")
		putString(t, c, "uploads", "img/cat.jpg", "meow")
		putString(t, c, "uploads", "doc/cat.png", "meow")
		if _, err := c.CopyObject(ctx, &s3.CopyObjectInput{Bucket: aws.String("uploads"), Key: aws.String("img/copy.png"), CopySource: aws.String("uploads/doc/cat.png")}); err != nil {
			t.Fatal(err)
		}

		created := s3Events(t, drain(t, queues, imagesURL))
		if len(created) != 2 {
			t.Fatalf("created events = %+v", created)
		}
		r := created[0].Records[0]
		if r.EventSource != "aws:s3" || r.EventName != "ObjectCreated:Put" || r.AwsRegion != "us-east-1" || r.S3.ConfigurationID != "pngs" ||
			r.S3.Bucket.Name != "uploads" || r.S3.Bucket.Arn != "arn:aws:s3:::uploads" || r.S3.Object.Key != "img/cat.png" || r.S3.Object.Size != 4 ||
			`"`+r.S3.Object.ETag+`"` != aws.ToString(put.ETag) {
			t.Fatalf("record = %+v", r)
		}
		if created[1].Records[0].EventName != "ObjectCreated:Copy" {
			t.Fatalf("copy record = %+v", created[1].Records[0])
		}

		if _, err := c.DeleteObject(ctx, &s3.DeleteObjectInput{Bucket: aws.String("uploads"), Key: aws.String("img/cat.jpg")}); err != nil {
			t.Fatal(err)
		}
		if _, err := c.DeleteObjects(ctx, &s3.DeleteObjectsInput{Bucket: aws.String("uploads"), Delete: &types.Delete{Objects: []types.ObjectIdentifier{{Key: aws.String("doc/cat.png")}, {Key: aws.String("img/cat.png")}}}}); err != nil {
			t.Fatal(err)
		}
		removed := s3Events(t, drain(t, queues, removedURL))
		if len(removed) != 3 || removed[0].Records[0].EventName != "ObjectRemoved:Delete" || removed[2].Records[0].S3.Object.Key != "img/cat.png" {
			t.Fatalf("removed events = %+v", removed)
		}
		if got := drain(t, queues, imagesURL); len(got) != 0 {
			t.Fatalf("deletes reached the created queue: %+v", got)
		}
	})
}

func TestS3NotificationToSNSAndEventBridge(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	c := s3Client(m, true)
	queues := sqs.NewFromConfig(m.Config())
	topics := sns.NewFromConfig(m.Config())
	events := eventbridge.NewFromConfig(m.Config())

	fanURL, fanArn := queueWithArn(t, queues, "fan", nil)
	busURL, busArn := queueWithArn(t, queues, "bus", nil)
	topic := mustTopic(t, topics, "uploads", nil)
	subscribeQueue(t, topics, topic, fanArn, nil)
	putRule(t, events, "s3-created", `{"source": ["aws.s3"], "detail-type": ["Object Created"], "detail": {"bucket": {"name": ["media"]}}}`)
	if _, err := events.PutTargets(ctx, &eventbridge.PutTargetsInput{Rule: aws.String("s3-created"), Targets: []ebtypes.Target{{Id: aws.String("q"), Arn: aws.String(busArn)}}}); err != nil {
		t.Fatal(err)
	}

	mustBucket(t, c, "media")
	if _, err := c.PutBucketNotificationConfiguration(ctx, &s3.PutBucketNotificationConfigurationInput{
		Bucket: aws.String("media"),
		NotificationConfiguration: &types.NotificationConfiguration{
			TopicConfigurations:      []types.TopicConfiguration{{TopicArn: aws.String(topic), Events: []types.Event{types.EventS3ObjectCreatedPut}}},
			EventBridgeConfiguration: &types.EventBridgeConfiguration{},
		},
	}); err != nil {
		t.Fatal(err)
	}
	drain(t, queues, fanURL)
	putString(t, c, "media", "a b.txt", "hello")

	fan := drain(t, queues, fanURL)
	if len(fan) != 1 {
		t.Fatalf("topic fan-out = %d", len(fan))
	}
	e := decodeEnvelope(t, aws.ToString(fan[0].Body))
	if e.Subject != "Amazon S3 Notification" || e.TopicArn != topic {
		t.Fatalf("envelope = %+v", e)
	}
	var inner s3Event
	if err := json.Unmarshal([]byte(e.Message), &inner); err != nil || inner.Records[0].S3.Object.Key != "a+b.txt" {
		t.Fatalf("inner = %+v %v", inner, err)
	}

	bus := drain(t, queues, busURL)
	if len(bus) != 1 {
		t.Fatalf("eventbridge delivery = %d", len(bus))
	}
	var ev struct {
		Source     string   `json:"source"`
		DetailType string   `json:"detail-type"`
		Resources  []string `json:"resources"`
		Detail     struct {
			Bucket struct{ Name string } `json:"bucket"`
			Object struct {
				Key  string `json:"key"`
				Size int    `json:"size"`
			} `json:"object"`
			Reason string `json:"reason"`
		} `json:"detail"`
	}
	if err := json.Unmarshal([]byte(aws.ToString(bus[0].Body)), &ev); err != nil {
		t.Fatal(err)
	}
	if ev.Source != "aws.s3" || ev.DetailType != "Object Created" || ev.Detail.Bucket.Name != "media" || ev.Detail.Object.Key != "a b.txt" || ev.Detail.Object.Size != 5 || ev.Detail.Reason != "PutObject" || len(ev.Resources) != 1 {
		t.Fatalf("event = %+v", ev)
	}
}

func TestDeliveryStaysInsideOneMock(t *testing.T) {
	a, b := newMock(t), newMock(t)
	topicsA := sns.NewFromConfig(a.Config())
	queuesB := sqs.NewFromConfig(b.Config())
	urlB, arnB := queueWithArn(t, queuesB, "shared-name", nil)
	_, arnA := queueWithArn(t, sqs.NewFromConfig(a.Config()), "shared-name", nil)
	if arnA != arnB {
		t.Fatalf("arns differ: %s %s", arnA, arnB)
	}
	topic := mustTopic(t, topicsA, "t", nil)
	subscribeQueue(t, topicsA, topic, arnA, nil)
	if _, err := topicsA.Publish(context.Background(), &sns.PublishInput{TopicArn: aws.String(topic), Message: aws.String("x")}); err != nil {
		t.Fatal(err)
	}
	if got := drain(t, queuesB, urlB); len(got) != 0 {
		t.Fatalf("delivery leaked into another mock: %+v", got)
	}
}

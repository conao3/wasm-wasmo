package wasmo_test

import (
	"context"
	"errors"
	"sync"
	"testing"

	"github.com/aws/aws-sdk-go-v2/aws"
	awshttp "github.com/aws/aws-sdk-go-v2/aws/transport/http"
	"github.com/aws/aws-sdk-go-v2/service/sqs"
	sqstypes "github.com/aws/aws-sdk-go-v2/service/sqs/types"
	"github.com/aws/aws-sdk-go-v2/service/sts"
	"github.com/aws/smithy-go"
	wasmo "github.com/conao3/wasm-wasmo/go"
)

func newMock(t testing.TB) *wasmo.Mock {
	t.Helper()
	ctx := context.Background()
	m, err := wasmo.New(ctx)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { m.Close(ctx) })
	return m
}

func apiCode(err error) string {
	var ae smithy.APIError
	if errors.As(err, &ae) {
		return ae.ErrorCode()
	}
	return ""
}

func httpStatus(err error) int {
	var re *awshttp.ResponseError
	if errors.As(err, &re) {
		return re.HTTPStatusCode()
	}
	return 0
}

func TestGetCallerIdentity(t *testing.T) {
	m := newMock(t)
	out, err := sts.NewFromConfig(m.Config()).GetCallerIdentity(context.Background(), &sts.GetCallerIdentityInput{})
	if err != nil {
		t.Fatal(err)
	}
	if got := aws.ToString(out.Account); got != "000000000000" {
		t.Fatalf("account = %q", got)
	}
}

func TestParallelMocks(t *testing.T) {
	var wg sync.WaitGroup
	for range 8 {
		wg.Add(1)
		go func() {
			defer wg.Done()
			m, err := wasmo.New(context.Background())
			if err != nil {
				t.Error(err)
				return
			}
			defer m.Close(context.Background())
			for range 50 {
				if _, err := sts.NewFromConfig(m.Config()).GetCallerIdentity(context.Background(), &sts.GetCallerIdentityInput{}); err != nil {
					t.Error(err)
					return
				}
			}
		}()
	}
	wg.Wait()
}

func BenchmarkGetCallerIdentity(b *testing.B) {
	m, err := wasmo.New(context.Background())
	if err != nil {
		b.Fatal(err)
	}
	client := sts.NewFromConfig(m.Config())
	for b.Loop() {
		if _, err := client.GetCallerIdentity(context.Background(), &sts.GetCallerIdentityInput{}); err != nil {
			b.Fatal(err)
		}
	}
}

func queueWithArn(t testing.TB, client *sqs.Client, name string, attrs map[string]string) (string, string) {
	t.Helper()
	ctx := context.Background()
	created, err := client.CreateQueue(ctx, &sqs.CreateQueueInput{QueueName: aws.String(name), Attributes: attrs})
	if err != nil {
		t.Fatal(err)
	}
	got, err := client.GetQueueAttributes(ctx, &sqs.GetQueueAttributesInput{
		QueueUrl:       created.QueueUrl,
		AttributeNames: []sqstypes.QueueAttributeName{sqstypes.QueueAttributeNameQueueArn},
	})
	if err != nil {
		t.Fatal(err)
	}
	return aws.ToString(created.QueueUrl), got.Attributes["QueueArn"]
}

func drain(t testing.TB, client *sqs.Client, url string) []sqstypes.Message {
	t.Helper()
	var all []sqstypes.Message
	for {
		out, err := client.ReceiveMessage(context.Background(), &sqs.ReceiveMessageInput{
			QueueUrl:              aws.String(url),
			MaxNumberOfMessages:   10,
			MessageAttributeNames: []string{"All"},
		})
		if err != nil {
			t.Fatal(err)
		}
		if len(out.Messages) == 0 {
			return all
		}
		all = append(all, out.Messages...)
	}
}

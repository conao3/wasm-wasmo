package wasmo_test

import (
	"context"
	"sync"
	"testing"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/sts"
	"github.com/conao3/wasm-wasmo/go"
)

func newMock(t *testing.T) *wasmo.Mock {
	t.Helper()
	ctx := context.Background()
	m, err := wasmo.New(ctx)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { m.Close(ctx) })
	return m
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

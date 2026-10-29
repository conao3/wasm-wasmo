package wasmo_test

import (
	"context"
	"crypto/md5"
	"errors"
	"fmt"
	"math/big"
	"testing"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/kinesis"
	"github.com/aws/aws-sdk-go-v2/service/kinesis/types"
)

func newKinesis(t *testing.T) *kinesis.Client {
	t.Helper()
	return kinesis.NewFromConfig(newMock(t).Config())
}

func createStream(t *testing.T, client *kinesis.Client, name string, shards int32) {
	t.Helper()
	ctx := context.Background()
	if _, err := client.CreateStream(ctx, &kinesis.CreateStreamInput{StreamName: aws.String(name), ShardCount: aws.Int32(shards)}); err != nil {
		t.Fatal(err)
	}
	if err := kinesis.NewStreamExistsWaiter(client, func(o *kinesis.StreamExistsWaiterOptions) {
		o.MinDelay = time.Millisecond
		o.MaxDelay = 5 * time.Millisecond
	}).Wait(ctx, &kinesis.DescribeStreamInput{StreamName: aws.String(name)}, time.Second); err != nil {
		t.Fatal(err)
	}
}

func readShard(t *testing.T, client *kinesis.Client, stream, shard string, typ types.ShardIteratorType, seq *string) ([]types.Record, *string) {
	t.Helper()
	ctx := context.Background()
	it, err := client.GetShardIterator(ctx, &kinesis.GetShardIteratorInput{
		StreamName:             aws.String(stream),
		ShardId:                aws.String(shard),
		ShardIteratorType:      typ,
		StartingSequenceNumber: seq,
	})
	if err != nil {
		t.Fatal(err)
	}
	out, err := client.GetRecords(ctx, &kinesis.GetRecordsInput{ShardIterator: it.ShardIterator})
	if err != nil {
		t.Fatal(err)
	}
	return out.Records, out.NextShardIterator
}

func shardFor(shards []types.Shard, key string) string {
	sum := md5.Sum([]byte(key))
	h := new(big.Int).SetBytes(sum[:])
	for _, s := range shards {
		lo, _ := new(big.Int).SetString(aws.ToString(s.HashKeyRange.StartingHashKey), 10)
		hi, _ := new(big.Int).SetString(aws.ToString(s.HashKeyRange.EndingHashKey), 10)
		if lo.Cmp(h) <= 0 && h.Cmp(hi) <= 0 {
			return aws.ToString(s.ShardId)
		}
	}
	return ""
}

func TestKinesisPutAndReadRecords(t *testing.T) {
	ctx := context.Background()
	client := newKinesis(t)
	createStream(t, client, "clicks", 4)

	shards, err := client.ListShards(ctx, &kinesis.ListShardsInput{StreamName: aws.String("clicks")})
	if err != nil {
		t.Fatal(err)
	}
	if len(shards.Shards) != 4 {
		t.Fatalf("shards = %d", len(shards.Shards))
	}

	var keys []string
	for i := range 20 {
		keys = append(keys, fmt.Sprintf("user-%d", i))
	}
	put, err := client.PutRecords(ctx, &kinesis.PutRecordsInput{
		StreamName: aws.String("clicks"),
		Records: func() []types.PutRecordsRequestEntry {
			var out []types.PutRecordsRequestEntry
			for _, k := range keys {
				out = append(out, types.PutRecordsRequestEntry{PartitionKey: aws.String(k), Data: []byte("click:" + k)})
			}
			return out
		}(),
	})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToInt32(put.FailedRecordCount) != 0 || len(put.Records) != len(keys) {
		t.Fatalf("put = %+v", put)
	}
	perShard := map[string]int{}
	for i, r := range put.Records {
		want := shardFor(shards.Shards, keys[i])
		if aws.ToString(r.ShardId) != want {
			t.Fatalf("%s went to %s, its hash belongs to %s", keys[i], aws.ToString(r.ShardId), want)
		}
		perShard[want]++
	}

	total := 0
	for _, s := range shards.Shards {
		records, next := readShard(t, client, "clicks", aws.ToString(s.ShardId), types.ShardIteratorTypeTrimHorizon, nil)
		if len(records) != perShard[aws.ToString(s.ShardId)] || next == nil {
			t.Fatalf("shard %s: %d records, want %d", aws.ToString(s.ShardId), len(records), perShard[aws.ToString(s.ShardId)])
		}
		for _, r := range records {
			if string(r.Data) != "click:"+aws.ToString(r.PartitionKey) {
				t.Fatalf("data = %q", r.Data)
			}
		}
		total += len(records)
	}
	if total != len(keys) {
		t.Fatalf("total = %d", total)
	}

	one, err := client.PutRecord(ctx, &kinesis.PutRecordInput{StreamName: aws.String("clicks"), PartitionKey: aws.String("any"), ExplicitHashKey: aws.String("0"), Data: []byte("first")})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(one.ShardId) != "shardId-000000000000" {
		t.Fatalf("explicit hash key 0 went to %s", aws.ToString(one.ShardId))
	}
	two, err := client.PutRecord(ctx, &kinesis.PutRecordInput{StreamName: aws.String("clicks"), PartitionKey: aws.String("any"), ExplicitHashKey: aws.String("0"), Data: []byte("second")})
	if err != nil {
		t.Fatal(err)
	}
	at, _ := readShard(t, client, "clicks", "shardId-000000000000", types.ShardIteratorTypeAtSequenceNumber, one.SequenceNumber)
	if len(at) != 2 || string(at[0].Data) != "first" {
		t.Fatalf("at-sequence = %d records", len(at))
	}
	after, _ := readShard(t, client, "clicks", "shardId-000000000000", types.ShardIteratorTypeAfterSequenceNumber, one.SequenceNumber)
	if len(after) != 1 || aws.ToString(after[0].SequenceNumber) != aws.ToString(two.SequenceNumber) {
		t.Fatalf("after-sequence = %+v", after)
	}
	latest, next := readShard(t, client, "clicks", "shardId-000000000000", types.ShardIteratorTypeLatest, nil)
	if len(latest) != 0 {
		t.Fatalf("latest = %d records", len(latest))
	}
	if _, err := client.PutRecord(ctx, &kinesis.PutRecordInput{StreamName: aws.String("clicks"), PartitionKey: aws.String("any"), ExplicitHashKey: aws.String("0"), Data: []byte("third")}); err != nil {
		t.Fatal(err)
	}
	more, err := client.GetRecords(ctx, &kinesis.GetRecordsInput{ShardIterator: next})
	if err != nil {
		t.Fatal(err)
	}
	if len(more.Records) != 1 || string(more.Records[0].Data) != "third" {
		t.Fatalf("records after latest = %+v", more.Records)
	}
	limited, err := client.GetShardIterator(ctx, &kinesis.GetShardIteratorInput{StreamName: aws.String("clicks"), ShardId: aws.String("shardId-000000000000"), ShardIteratorType: types.ShardIteratorTypeTrimHorizon})
	if err != nil {
		t.Fatal(err)
	}
	page, err := client.GetRecords(ctx, &kinesis.GetRecordsInput{ShardIterator: limited.ShardIterator, Limit: aws.Int32(1)})
	if err != nil {
		t.Fatal(err)
	}
	if len(page.Records) != 1 {
		t.Fatalf("limit 1 returned %d", len(page.Records))
	}
}

func TestKinesisResharding(t *testing.T) {
	ctx := context.Background()
	client := newKinesis(t)
	createStream(t, client, "orders", 1)

	if _, err := client.PutRecord(ctx, &kinesis.PutRecordInput{StreamName: aws.String("orders"), PartitionKey: aws.String("a"), Data: []byte("before")}); err != nil {
		t.Fatal(err)
	}
	half := new(big.Int).Lsh(big.NewInt(1), 127).String()
	if _, err := client.SplitShard(ctx, &kinesis.SplitShardInput{StreamName: aws.String("orders"), ShardToSplit: aws.String("shardId-000000000000"), NewStartingHashKey: aws.String(half)}); err != nil {
		t.Fatal(err)
	}
	shards, err := client.ListShards(ctx, &kinesis.ListShardsInput{StreamName: aws.String("orders")})
	if err != nil {
		t.Fatal(err)
	}
	if len(shards.Shards) != 3 || shards.Shards[0].SequenceNumberRange.EndingSequenceNumber == nil {
		t.Fatalf("shards after split = %+v", shards.Shards)
	}
	if aws.ToString(shards.Shards[1].ParentShardId) != "shardId-000000000000" || aws.ToString(shards.Shards[2].HashKeyRange.StartingHashKey) != half {
		t.Fatalf("children = %+v", shards.Shards[1:])
	}

	records, next := readShard(t, client, "orders", "shardId-000000000000", types.ShardIteratorTypeTrimHorizon, nil)
	if len(records) != 1 || next != nil {
		t.Fatalf("closed parent: %d records, next %v", len(records), next)
	}

	if _, err := client.MergeShards(ctx, &kinesis.MergeShardsInput{StreamName: aws.String("orders"), ShardToMerge: aws.String("shardId-000000000001"), AdjacentShardToMerge: aws.String("shardId-000000000002")}); err != nil {
		t.Fatal(err)
	}
	summary, err := client.DescribeStreamSummary(ctx, &kinesis.DescribeStreamSummaryInput{StreamName: aws.String("orders")})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToInt32(summary.StreamDescriptionSummary.OpenShardCount) != 1 {
		t.Fatalf("open shards = %d", aws.ToInt32(summary.StreamDescriptionSummary.OpenShardCount))
	}
	open, err := client.ListShards(ctx, &kinesis.ListShardsInput{StreamName: aws.String("orders"), ShardFilter: &types.ShardFilter{Type: types.ShardFilterTypeAtLatest}})
	if err != nil {
		t.Fatal(err)
	}
	if len(open.Shards) != 1 || aws.ToString(open.Shards[0].AdjacentParentShardId) != "shardId-000000000002" {
		t.Fatalf("open shards = %+v", open.Shards)
	}

	updated, err := client.UpdateShardCount(ctx, &kinesis.UpdateShardCountInput{StreamName: aws.String("orders"), TargetShardCount: aws.Int32(2), ScalingType: types.ScalingTypeUniformScaling})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToInt32(updated.CurrentShardCount) != 1 || aws.ToInt32(updated.TargetShardCount) != 2 {
		t.Fatalf("update = %+v", updated)
	}
}

func TestKinesisStreamAdministration(t *testing.T) {
	ctx := context.Background()
	client := newKinesis(t)
	createStream(t, client, "audit", 1)
	createStream(t, client, "billing", 1)

	listed, err := client.ListStreams(ctx, &kinesis.ListStreamsInput{})
	if err != nil {
		t.Fatal(err)
	}
	if len(listed.StreamNames) != 2 || listed.StreamNames[0] != "audit" {
		t.Fatalf("streams = %v", listed.StreamNames)
	}

	if _, err := client.AddTagsToStream(ctx, &kinesis.AddTagsToStreamInput{StreamName: aws.String("audit"), Tags: map[string]string{"team": "core", "env": "test"}}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.RemoveTagsFromStream(ctx, &kinesis.RemoveTagsFromStreamInput{StreamName: aws.String("audit"), TagKeys: []string{"env"}}); err != nil {
		t.Fatal(err)
	}
	tags, err := client.ListTagsForStream(ctx, &kinesis.ListTagsForStreamInput{StreamName: aws.String("audit")})
	if err != nil {
		t.Fatal(err)
	}
	if len(tags.Tags) != 1 || aws.ToString(tags.Tags[0].Key) != "team" {
		t.Fatalf("tags = %+v", tags.Tags)
	}

	if _, err := client.IncreaseStreamRetentionPeriod(ctx, &kinesis.IncreaseStreamRetentionPeriodInput{StreamName: aws.String("audit"), RetentionPeriodHours: aws.Int32(48)}); err != nil {
		t.Fatal(err)
	}
	_, err = client.DecreaseStreamRetentionPeriod(ctx, &kinesis.DecreaseStreamRetentionPeriodInput{StreamName: aws.String("audit"), RetentionPeriodHours: aws.Int32(72)})
	var invalid *types.InvalidArgumentException
	if !errors.As(err, &invalid) {
		t.Fatalf("decrease above current: %v", err)
	}

	desc, err := client.DescribeStream(ctx, &kinesis.DescribeStreamInput{StreamName: aws.String("audit")})
	if err != nil {
		t.Fatal(err)
	}
	arn := desc.StreamDescription.StreamARN
	if aws.ToInt32(desc.StreamDescription.RetentionPeriodHours) != 48 {
		t.Fatalf("retention = %d", aws.ToInt32(desc.StreamDescription.RetentionPeriodHours))
	}
	consumer, err := client.RegisterStreamConsumer(ctx, &kinesis.RegisterStreamConsumerInput{StreamARN: arn, ConsumerName: aws.String("reader")})
	if err != nil {
		t.Fatal(err)
	}
	got, err := client.DescribeStreamConsumer(ctx, &kinesis.DescribeStreamConsumerInput{ConsumerARN: consumer.Consumer.ConsumerARN})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(got.ConsumerDescription.ConsumerName) != "reader" || aws.ToString(got.ConsumerDescription.StreamARN) != aws.ToString(arn) {
		t.Fatalf("consumer = %+v", got.ConsumerDescription)
	}
	var inUse *types.ResourceInUseException
	if _, err := client.DeleteStream(ctx, &kinesis.DeleteStreamInput{StreamName: aws.String("audit")}); !errors.As(err, &inUse) {
		t.Fatalf("delete with consumers: %v", err)
	}
	if _, err := client.DeleteStream(ctx, &kinesis.DeleteStreamInput{StreamName: aws.String("audit"), EnforceConsumerDeletion: aws.Bool(true)}); err != nil {
		t.Fatal(err)
	}

	var nf *types.ResourceNotFoundException
	if _, err := client.PutRecord(ctx, &kinesis.PutRecordInput{StreamName: aws.String("audit"), PartitionKey: aws.String("k"), Data: []byte("x")}); !errors.As(err, &nf) {
		t.Fatalf("put to deleted stream: %v", err)
	}
	if _, err := client.CreateStream(ctx, &kinesis.CreateStreamInput{StreamName: aws.String("billing"), ShardCount: aws.Int32(1)}); !errors.As(err, &inUse) {
		t.Fatalf("duplicate create: %v", err)
	}
}

package wasmo_test

import (
	"bytes"
	"compress/gzip"
	"context"
	"errors"
	"io"
	"strings"
	"testing"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/firehose"
	"github.com/aws/aws-sdk-go-v2/service/firehose/types"
	"github.com/aws/aws-sdk-go-v2/service/s3"
	wasmo "github.com/conao3/wasm-wasmo/go"
)

func s3Delivery(t *testing.T, m *wasmo.Mock, name, bucket, prefix string, compression types.CompressionFormat) *firehose.Client {
	t.Helper()
	ctx := context.Background()
	mustBucket(t, s3Client(m, true), bucket)
	client := firehose.NewFromConfig(m.Config())
	if _, err := client.CreateDeliveryStream(ctx, &firehose.CreateDeliveryStreamInput{
		DeliveryStreamName: aws.String(name),
		ExtendedS3DestinationConfiguration: &types.ExtendedS3DestinationConfiguration{
			BucketARN:         aws.String("arn:aws:s3:::" + bucket),
			RoleARN:           aws.String("arn:aws:iam::000000000000:role/firehose"),
			Prefix:            aws.String(prefix),
			CompressionFormat: compression,
		},
	}); err != nil {
		t.Fatal(err)
	}
	return client
}

func objectsUnder(t *testing.T, c *s3.Client, bucket, prefix string) map[string][]byte {
	t.Helper()
	ctx := context.Background()
	listed, err := c.ListObjectsV2(ctx, &s3.ListObjectsV2Input{Bucket: aws.String(bucket), Prefix: aws.String(prefix)})
	if err != nil {
		t.Fatal(err)
	}
	out := map[string][]byte{}
	for _, o := range listed.Contents {
		got, err := c.GetObject(ctx, &s3.GetObjectInput{Bucket: aws.String(bucket), Key: o.Key})
		if err != nil {
			t.Fatal(err)
		}
		out[aws.ToString(o.Key)] = readAll(t, got.Body)
	}
	return out
}

func TestFirehoseDeliversToS3(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	client := s3Delivery(t, m, "events", "lake", "raw/", types.CompressionFormatUncompressed)
	s3c := s3Client(m, true)

	put, err := client.PutRecord(ctx, &firehose.PutRecordInput{DeliveryStreamName: aws.String("events"), Record: &types.Record{Data: []byte("one\n")}})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(put.RecordId) == "" {
		t.Fatal("no record id")
	}
	batch, err := client.PutRecordBatch(ctx, &firehose.PutRecordBatchInput{
		DeliveryStreamName: aws.String("events"),
		Records:            []types.Record{{Data: []byte("two\n")}, {Data: []byte("three\n")}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToInt32(batch.FailedPutCount) != 0 || len(batch.RequestResponses) != 2 {
		t.Fatalf("batch = %+v", batch)
	}

	objects := objectsUnder(t, s3c, "lake", "raw/")
	if len(objects) != 2 {
		t.Fatalf("objects = %v", objects)
	}
	var bodies []string
	for key, body := range objects {
		parts := strings.Split(strings.TrimPrefix(key, "raw/"), "/")
		if len(parts) != 5 || !strings.HasPrefix(parts[4], "events-1-") {
			t.Fatalf("key %q does not follow prefix/YYYY/MM/DD/HH/stream-version-...", key)
		}
		bodies = append(bodies, string(body))
	}
	joined := strings.Join(bodies, "|")
	if !strings.Contains(joined, "one\n") || !strings.Contains(joined, "two\nthree\n") {
		t.Fatalf("bodies = %q (a batch lands in one object)", bodies)
	}
}

func TestFirehoseGzipAndDestinationUpdates(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	client := s3Delivery(t, m, "metrics", "archive", "gz/", types.CompressionFormatGzip)
	if _, err := client.PutRecord(ctx, &firehose.PutRecordInput{DeliveryStreamName: aws.String("metrics"), Record: &types.Record{Data: []byte("compressed payload")}}); err != nil {
		t.Fatal(err)
	}
	for key, body := range objectsUnder(t, s3Client(m, true), "archive", "gz/") {
		if !strings.HasSuffix(key, ".gz") {
			t.Fatalf("key = %q", key)
		}
		r, err := gzip.NewReader(bytes.NewReader(body))
		if err != nil {
			t.Fatal(err)
		}
		plain, _ := io.ReadAll(r)
		if string(plain) != "compressed payload" {
			t.Fatalf("payload = %q", plain)
		}
	}

	desc, err := client.DescribeDeliveryStream(ctx, &firehose.DescribeDeliveryStreamInput{DeliveryStreamName: aws.String("metrics")})
	if err != nil {
		t.Fatal(err)
	}
	d := desc.DeliveryStreamDescription
	if d.DeliveryStreamStatus != types.DeliveryStreamStatusActive || len(d.Destinations) != 1 {
		t.Fatalf("description = %+v", d)
	}
	ext := d.Destinations[0].ExtendedS3DestinationDescription
	if aws.ToInt32(ext.BufferingHints.IntervalInSeconds) != 300 || aws.ToString(ext.Prefix) != "gz/" {
		t.Fatalf("destination = %+v", ext)
	}

	_, err = client.UpdateDestination(ctx, &firehose.UpdateDestinationInput{
		DeliveryStreamName:             aws.String("metrics"),
		CurrentDeliveryStreamVersionId: aws.String("7"),
		DestinationId:                  d.Destinations[0].DestinationId,
		ExtendedS3DestinationUpdate:    &types.ExtendedS3DestinationUpdate{Prefix: aws.String("v2/")},
	})
	var conflict *types.ConcurrentModificationException
	if !errors.As(err, &conflict) {
		t.Fatalf("stale version: %v", err)
	}
	if _, err := client.UpdateDestination(ctx, &firehose.UpdateDestinationInput{
		DeliveryStreamName:             aws.String("metrics"),
		CurrentDeliveryStreamVersionId: d.VersionId,
		DestinationId:                  d.Destinations[0].DestinationId,
		ExtendedS3DestinationUpdate:    &types.ExtendedS3DestinationUpdate{Prefix: aws.String("v2/"), CompressionFormat: types.CompressionFormatUncompressed},
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.PutRecord(ctx, &firehose.PutRecordInput{DeliveryStreamName: aws.String("metrics"), Record: &types.Record{Data: []byte("plain")}}); err != nil {
		t.Fatal(err)
	}
	for key, body := range objectsUnder(t, s3Client(m, true), "archive", "v2/") {
		if !strings.Contains(key, "metrics-2-") || string(body) != "plain" {
			t.Fatalf("%s = %q", key, body)
		}
	}
}

func TestFirehoseStreamAdministration(t *testing.T) {
	ctx := context.Background()
	client := firehose.NewFromConfig(newMock(t).Config())
	for _, name := range []string{"b-stream", "a-stream"} {
		if _, err := client.CreateDeliveryStream(ctx, &firehose.CreateDeliveryStreamInput{
			DeliveryStreamName: aws.String(name),
			Tags:               []types.Tag{{Key: aws.String("team"), Value: aws.String("data")}},
		}); err != nil {
			t.Fatal(err)
		}
	}
	listed, err := client.ListDeliveryStreams(ctx, &firehose.ListDeliveryStreamsInput{Limit: aws.Int32(1)})
	if err != nil {
		t.Fatal(err)
	}
	if len(listed.DeliveryStreamNames) != 1 || listed.DeliveryStreamNames[0] != "a-stream" || !aws.ToBool(listed.HasMoreDeliveryStreams) {
		t.Fatalf("listed = %+v", listed)
	}

	if _, err := client.TagDeliveryStream(ctx, &firehose.TagDeliveryStreamInput{DeliveryStreamName: aws.String("a-stream"), Tags: []types.Tag{{Key: aws.String("env"), Value: aws.String("test")}}}); err != nil {
		t.Fatal(err)
	}
	if _, err := client.UntagDeliveryStream(ctx, &firehose.UntagDeliveryStreamInput{DeliveryStreamName: aws.String("a-stream"), TagKeys: []string{"team"}}); err != nil {
		t.Fatal(err)
	}
	tags, err := client.ListTagsForDeliveryStream(ctx, &firehose.ListTagsForDeliveryStreamInput{DeliveryStreamName: aws.String("a-stream")})
	if err != nil {
		t.Fatal(err)
	}
	if len(tags.Tags) != 1 || aws.ToString(tags.Tags[0].Key) != "env" {
		t.Fatalf("tags = %+v", tags.Tags)
	}

	if _, err := client.StartDeliveryStreamEncryption(ctx, &firehose.StartDeliveryStreamEncryptionInput{
		DeliveryStreamName:                         aws.String("a-stream"),
		DeliveryStreamEncryptionConfigurationInput: &types.DeliveryStreamEncryptionConfigurationInput{KeyType: types.KeyTypeAwsOwnedCmk},
	}); err != nil {
		t.Fatal(err)
	}
	desc, err := client.DescribeDeliveryStream(ctx, &firehose.DescribeDeliveryStreamInput{DeliveryStreamName: aws.String("a-stream")})
	if err != nil {
		t.Fatal(err)
	}
	if desc.DeliveryStreamDescription.DeliveryStreamEncryptionConfiguration.Status != types.DeliveryStreamEncryptionStatusEnabled {
		t.Fatalf("encryption = %+v", desc.DeliveryStreamDescription.DeliveryStreamEncryptionConfiguration)
	}

	var inUse *types.ResourceInUseException
	if _, err := client.CreateDeliveryStream(ctx, &firehose.CreateDeliveryStreamInput{DeliveryStreamName: aws.String("a-stream")}); !errors.As(err, &inUse) {
		t.Fatalf("duplicate: %v", err)
	}
	if _, err := client.DeleteDeliveryStream(ctx, &firehose.DeleteDeliveryStreamInput{DeliveryStreamName: aws.String("a-stream")}); err != nil {
		t.Fatal(err)
	}
	var nf *types.ResourceNotFoundException
	if _, err := client.PutRecord(ctx, &firehose.PutRecordInput{DeliveryStreamName: aws.String("a-stream"), Record: &types.Record{Data: []byte("x")}}); !errors.As(err, &nf) {
		t.Fatalf("put to deleted stream: %v", err)
	}
}

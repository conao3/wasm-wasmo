package wasmo_test

import (
	"context"
	"errors"
	"testing"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/dynamodb"
	"github.com/aws/aws-sdk-go-v2/service/dynamodb/types"
	"github.com/aws/aws-sdk-go-v2/service/dynamodbstreams"
	streamtypes "github.com/aws/aws-sdk-go-v2/service/dynamodbstreams/types"
)

func streamTable(t *testing.T, client *dynamodb.Client, name string, view types.StreamViewType) string {
	t.Helper()
	out, err := client.CreateTable(context.Background(), &dynamodb.CreateTableInput{
		TableName:            aws.String(name),
		AttributeDefinitions: []types.AttributeDefinition{{AttributeName: aws.String("id"), AttributeType: types.ScalarAttributeTypeS}},
		KeySchema:            []types.KeySchemaElement{{AttributeName: aws.String("id"), KeyType: types.KeyTypeHash}},
		BillingMode:          types.BillingModePayPerRequest,
		StreamSpecification:  &types.StreamSpecification{StreamEnabled: aws.Bool(true), StreamViewType: view},
	})
	if err != nil {
		t.Fatal(err)
	}
	return aws.ToString(out.TableDescription.LatestStreamArn)
}

func streamRecords(t *testing.T, client *dynamodbstreams.Client, arn string) []streamtypes.Record {
	t.Helper()
	ctx := context.Background()
	desc, err := client.DescribeStream(ctx, &dynamodbstreams.DescribeStreamInput{StreamArn: aws.String(arn)})
	if err != nil {
		t.Fatal(err)
	}
	it, err := client.GetShardIterator(ctx, &dynamodbstreams.GetShardIteratorInput{
		StreamArn:         aws.String(arn),
		ShardId:           desc.StreamDescription.Shards[0].ShardId,
		ShardIteratorType: streamtypes.ShardIteratorTypeTrimHorizon,
	})
	if err != nil {
		t.Fatal(err)
	}
	out, err := client.GetRecords(ctx, &dynamodbstreams.GetRecordsInput{ShardIterator: it.ShardIterator})
	if err != nil {
		t.Fatal(err)
	}
	if out.NextShardIterator == nil {
		t.Fatal("open shard returned no next iterator")
	}
	return out.Records
}

func TestDynamoDBStreamsRecordWrites(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	db := dynamodb.NewFromConfig(m.Config())
	streams := dynamodbstreams.NewFromConfig(m.Config())
	arn := streamTable(t, db, "users", types.StreamViewTypeNewAndOldImages)

	if _, err := db.PutItem(ctx, &dynamodb.PutItemInput{TableName: aws.String("users"), Item: map[string]types.AttributeValue{"id": s("u1"), "name": s("ada")}}); err != nil {
		t.Fatal(err)
	}
	if _, err := db.UpdateItem(ctx, &dynamodb.UpdateItemInput{
		TableName:                 aws.String("users"),
		Key:                       map[string]types.AttributeValue{"id": s("u1")},
		UpdateExpression:          aws.String("SET #n = :n"),
		ExpressionAttributeNames:  map[string]string{"#n": "name"},
		ExpressionAttributeValues: map[string]types.AttributeValue{":n": s("grace")},
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := db.PutItem(ctx, &dynamodb.PutItemInput{TableName: aws.String("users"), Item: map[string]types.AttributeValue{"id": s("u1"), "name": s("grace")}}); err != nil {
		t.Fatal(err)
	}
	if _, err := db.DeleteItem(ctx, &dynamodb.DeleteItemInput{TableName: aws.String("users"), Key: map[string]types.AttributeValue{"id": s("u1")}}); err != nil {
		t.Fatal(err)
	}

	records := streamRecords(t, streams, arn)
	if len(records) != 3 {
		t.Fatalf("records = %d, want 3 (an unchanged put writes none)", len(records))
	}
	want := []streamtypes.OperationType{streamtypes.OperationTypeInsert, streamtypes.OperationTypeModify, streamtypes.OperationTypeRemove}
	for i, r := range records {
		if r.EventName != want[i] {
			t.Fatalf("record %d event = %s, want %s", i, r.EventName, want[i])
		}
		if aws.ToString(r.EventSource) != "aws:dynamodb" {
			t.Fatalf("event source = %q", aws.ToString(r.EventSource))
		}
		key := r.Dynamodb.Keys["id"].(*streamtypes.AttributeValueMemberS).Value
		if key != "u1" {
			t.Fatalf("key = %q", key)
		}
	}
	name := func(img map[string]streamtypes.AttributeValue) string {
		return img["name"].(*streamtypes.AttributeValueMemberS).Value
	}
	if records[0].Dynamodb.OldImage != nil || name(records[0].Dynamodb.NewImage) != "ada" {
		t.Fatalf("insert images = %v / %v", records[0].Dynamodb.OldImage, records[0].Dynamodb.NewImage)
	}
	if name(records[1].Dynamodb.OldImage) != "ada" || name(records[1].Dynamodb.NewImage) != "grace" {
		t.Fatal("modify record lacks the old and new images")
	}
	if records[2].Dynamodb.NewImage != nil || name(records[2].Dynamodb.OldImage) != "grace" {
		t.Fatal("remove record should carry only the old image")
	}
	if aws.ToString(records[0].Dynamodb.SequenceNumber) >= aws.ToString(records[1].Dynamodb.SequenceNumber) {
		t.Fatal("sequence numbers do not increase")
	}

	desc, err := streams.DescribeStream(ctx, &dynamodbstreams.DescribeStreamInput{StreamArn: aws.String(arn)})
	if err != nil {
		t.Fatal(err)
	}
	after, err := streams.GetShardIterator(ctx, &dynamodbstreams.GetShardIteratorInput{
		StreamArn:         aws.String(arn),
		ShardId:           desc.StreamDescription.Shards[0].ShardId,
		ShardIteratorType: streamtypes.ShardIteratorTypeAfterSequenceNumber,
		SequenceNumber:    records[1].Dynamodb.SequenceNumber,
	})
	if err != nil {
		t.Fatal(err)
	}
	rest, err := streams.GetRecords(ctx, &dynamodbstreams.GetRecordsInput{ShardIterator: after.ShardIterator})
	if err != nil {
		t.Fatal(err)
	}
	if len(rest.Records) != 1 || rest.Records[0].EventName != streamtypes.OperationTypeRemove {
		t.Fatalf("after-sequence records = %+v", rest.Records)
	}
}

func TestDynamoDBStreamsViewTypesAndListing(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	db := dynamodb.NewFromConfig(m.Config())
	streams := dynamodbstreams.NewFromConfig(m.Config())
	keysArn := streamTable(t, db, "keys", types.StreamViewTypeKeysOnly)
	newArn := streamTable(t, db, "news", types.StreamViewTypeNewImage)

	for _, table := range []string{"keys", "news"} {
		if _, err := db.BatchWriteItem(ctx, &dynamodb.BatchWriteItemInput{RequestItems: map[string][]types.WriteRequest{
			table: {
				{PutRequest: &types.PutRequest{Item: map[string]types.AttributeValue{"id": s("a"), "v": n("1")}}},
				{PutRequest: &types.PutRequest{Item: map[string]types.AttributeValue{"id": s("b"), "v": n("2")}}},
			},
		}}); err != nil {
			t.Fatal(err)
		}
	}
	for _, r := range streamRecords(t, streams, keysArn) {
		if r.Dynamodb.NewImage != nil || r.Dynamodb.OldImage != nil || r.Dynamodb.StreamViewType != streamtypes.StreamViewTypeKeysOnly {
			t.Fatalf("keys-only record carries images: %+v", r.Dynamodb)
		}
	}
	news := streamRecords(t, streams, newArn)
	if len(news) != 2 || news[1].Dynamodb.NewImage["v"].(*streamtypes.AttributeValueMemberN).Value != "2" {
		t.Fatalf("new-image records = %+v", news)
	}

	listed, err := streams.ListStreams(ctx, &dynamodbstreams.ListStreamsInput{TableName: aws.String("news")})
	if err != nil {
		t.Fatal(err)
	}
	if len(listed.Streams) != 1 || aws.ToString(listed.Streams[0].StreamArn) != newArn {
		t.Fatalf("streams = %+v", listed.Streams)
	}

	if _, err := db.UpdateTable(ctx, &dynamodb.UpdateTableInput{TableName: aws.String("news"), StreamSpecification: &types.StreamSpecification{StreamEnabled: aws.Bool(false)}}); err != nil {
		t.Fatal(err)
	}
	desc, err := streams.DescribeStream(ctx, &dynamodbstreams.DescribeStreamInput{StreamArn: aws.String(newArn)})
	if err != nil {
		t.Fatal(err)
	}
	if desc.StreamDescription.StreamStatus != streamtypes.StreamStatusDisabled || desc.StreamDescription.Shards[0].SequenceNumberRange.EndingSequenceNumber == nil {
		t.Fatalf("disabled stream = %+v", desc.StreamDescription)
	}

	_, err = streams.DescribeStream(ctx, &dynamodbstreams.DescribeStreamInput{StreamArn: aws.String(newArn + "x")})
	var nf *streamtypes.ResourceNotFoundException
	if !errors.As(err, &nf) {
		t.Fatalf("err = %v", err)
	}
}

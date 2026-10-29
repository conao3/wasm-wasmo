package wasmo_test

import (
	"context"
	"errors"
	"fmt"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/feature/dynamodb/attributevalue"
	"github.com/aws/aws-sdk-go-v2/service/dynamodb"
	"github.com/aws/aws-sdk-go-v2/service/dynamodb/types"
)

func ddb(t testing.TB) *dynamodb.Client {
	t.Helper()
	return dynamodb.NewFromConfig(newMock(t).Config())
}

func s(v string) types.AttributeValue    { return &types.AttributeValueMemberS{Value: v} }
func n(v string) types.AttributeValue    { return &types.AttributeValueMemberN{Value: v} }
func str(av types.AttributeValue) string { return av.(*types.AttributeValueMemberS).Value }
func num(av types.AttributeValue) string { return av.(*types.AttributeValueMemberN).Value }

func createOrders(t testing.TB, client *dynamodb.Client) {
	t.Helper()
	ctx := context.Background()
	_, err := client.CreateTable(ctx, &dynamodb.CreateTableInput{
		TableName: aws.String("orders"),
		AttributeDefinitions: []types.AttributeDefinition{
			{AttributeName: aws.String("customer"), AttributeType: types.ScalarAttributeTypeS},
			{AttributeName: aws.String("order"), AttributeType: types.ScalarAttributeTypeN},
			{AttributeName: aws.String("status"), AttributeType: types.ScalarAttributeTypeS},
			{AttributeName: aws.String("placedAt"), AttributeType: types.ScalarAttributeTypeS},
		},
		KeySchema: []types.KeySchemaElement{
			{AttributeName: aws.String("customer"), KeyType: types.KeyTypeHash},
			{AttributeName: aws.String("order"), KeyType: types.KeyTypeRange},
		},
		GlobalSecondaryIndexes: []types.GlobalSecondaryIndex{{
			IndexName: aws.String("by-status"),
			KeySchema: []types.KeySchemaElement{
				{AttributeName: aws.String("status"), KeyType: types.KeyTypeHash},
				{AttributeName: aws.String("placedAt"), KeyType: types.KeyTypeRange},
			},
			Projection: &types.Projection{ProjectionType: types.ProjectionTypeKeysOnly},
		}},
		BillingMode: types.BillingModePayPerRequest,
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := dynamodb.NewTableExistsWaiter(client, func(o *dynamodb.TableExistsWaiterOptions) {
		o.MinDelay = time.Millisecond
		o.MaxDelay = 5 * time.Millisecond
	}).Wait(ctx, &dynamodb.DescribeTableInput{TableName: aws.String("orders")}, time.Second); err != nil {
		t.Fatal(err)
	}
}

func TestDynamoDBTableLifecycle(t *testing.T) {
	ctx := context.Background()
	client := ddb(t)

	created, err := client.CreateTable(ctx, &dynamodb.CreateTableInput{
		TableName:             aws.String("events"),
		AttributeDefinitions:  []types.AttributeDefinition{{AttributeName: aws.String("id"), AttributeType: types.ScalarAttributeTypeS}},
		KeySchema:             []types.KeySchemaElement{{AttributeName: aws.String("id"), KeyType: types.KeyTypeHash}},
		ProvisionedThroughput: &types.ProvisionedThroughput{ReadCapacityUnits: aws.Int64(5), WriteCapacityUnits: aws.Int64(5)},
		Tags:                  []types.Tag{{Key: aws.String("env"), Value: aws.String("test")}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if created.TableDescription.TableStatus != types.TableStatusCreating {
		t.Fatalf("status = %s", created.TableDescription.TableStatus)
	}
	if err := dynamodb.NewTableExistsWaiter(client).Wait(ctx, &dynamodb.DescribeTableInput{TableName: aws.String("events")}, 5*time.Second); err != nil {
		t.Fatal(err)
	}
	if _, err := client.CreateTable(ctx, &dynamodb.CreateTableInput{
		TableName:            aws.String("events"),
		AttributeDefinitions: []types.AttributeDefinition{{AttributeName: aws.String("id"), AttributeType: types.ScalarAttributeTypeS}},
		KeySchema:            []types.KeySchemaElement{{AttributeName: aws.String("id"), KeyType: types.KeyTypeHash}},
		BillingMode:          types.BillingModePayPerRequest,
	}); !errors.As(err, new(*types.ResourceInUseException)) {
		t.Fatalf("duplicate create: %v", err)
	}
	createOrders(t, client)
	for _, name := range []string{"audit", "carts"} {
		if _, err := client.CreateTable(ctx, &dynamodb.CreateTableInput{
			TableName:            aws.String(name),
			AttributeDefinitions: []types.AttributeDefinition{{AttributeName: aws.String("id"), AttributeType: types.ScalarAttributeTypeS}},
			KeySchema:            []types.KeySchemaElement{{AttributeName: aws.String("id"), KeyType: types.KeyTypeHash}},
			BillingMode:          types.BillingModePayPerRequest,
		}); err != nil {
			t.Fatal(err)
		}
	}

	var names []string
	pages := 0
	for p := dynamodb.NewListTablesPaginator(client, &dynamodb.ListTablesInput{Limit: aws.Int32(3)}); p.HasMorePages(); {
		out, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		pages++
		names = append(names, out.TableNames...)
	}
	if !slices.Equal(names, []string{"audit", "carts", "events", "orders"}) || pages != 2 {
		t.Fatalf("tables = %v in %d pages", names, pages)
	}

	updated, err := client.UpdateTable(ctx, &dynamodb.UpdateTableInput{
		TableName:             aws.String("events"),
		ProvisionedThroughput: &types.ProvisionedThroughput{ReadCapacityUnits: aws.Int64(10), WriteCapacityUnits: aws.Int64(7)},
		AttributeDefinitions:  []types.AttributeDefinition{{AttributeName: aws.String("kind"), AttributeType: types.ScalarAttributeTypeS}},
		GlobalSecondaryIndexUpdates: []types.GlobalSecondaryIndexUpdate{{Create: &types.CreateGlobalSecondaryIndexAction{
			IndexName:             aws.String("by-kind"),
			KeySchema:             []types.KeySchemaElement{{AttributeName: aws.String("kind"), KeyType: types.KeyTypeHash}},
			Projection:            &types.Projection{ProjectionType: types.ProjectionTypeAll},
			ProvisionedThroughput: &types.ProvisionedThroughput{ReadCapacityUnits: aws.Int64(1), WriteCapacityUnits: aws.Int64(1)},
		}}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if gsi := updated.TableDescription.GlobalSecondaryIndexes; len(gsi) != 1 || gsi[0].IndexStatus != types.IndexStatusCreating {
		t.Fatalf("gsi = %+v", gsi)
	}
	desc, err := client.DescribeTable(ctx, &dynamodb.DescribeTableInput{TableName: aws.String("events")})
	if err != nil {
		t.Fatal(err)
	}
	tbl := desc.Table
	if tbl.TableStatus != types.TableStatusActive || tbl.GlobalSecondaryIndexes[0].IndexStatus != types.IndexStatusActive ||
		aws.ToInt64(tbl.ProvisionedThroughput.ReadCapacityUnits) != 10 || len(tbl.AttributeDefinitions) != 2 {
		t.Fatalf("described = %+v", tbl)
	}

	tags, err := client.ListTagsOfResource(ctx, &dynamodb.ListTagsOfResourceInput{ResourceArn: tbl.TableArn})
	if err != nil || len(tags.Tags) != 1 || aws.ToString(tags.Tags[0].Value) != "test" {
		t.Fatalf("tags = %+v, %v", tags, err)
	}
	if _, err := client.UpdateTimeToLive(ctx, &dynamodb.UpdateTimeToLiveInput{
		TableName:               aws.String("events"),
		TimeToLiveSpecification: &types.TimeToLiveSpecification{AttributeName: aws.String("expiresAt"), Enabled: aws.Bool(true)},
	}); err != nil {
		t.Fatal(err)
	}
	past := fmt.Sprint(time.Now().Add(-time.Hour).Unix())
	if _, err := client.PutItem(ctx, &dynamodb.PutItemInput{
		TableName: aws.String("events"),
		Item:      map[string]types.AttributeValue{"id": s("old"), "expiresAt": n(past)},
	}); err != nil {
		t.Fatal(err)
	}
	if got, err := client.GetItem(ctx, &dynamodb.GetItemInput{TableName: aws.String("events"), Key: map[string]types.AttributeValue{"id": s("old")}}); err != nil || got.Item != nil {
		t.Fatalf("expired item = %v, %v", got.Item, err)
	}

	if _, err := client.DeleteTable(ctx, &dynamodb.DeleteTableInput{TableName: aws.String("events")}); err != nil {
		t.Fatal(err)
	}
	if err := dynamodb.NewTableNotExistsWaiter(client).Wait(ctx, &dynamodb.DescribeTableInput{TableName: aws.String("events")}, 5*time.Second); err != nil {
		t.Fatal(err)
	}
	if _, err := client.GetItem(ctx, &dynamodb.GetItemInput{TableName: aws.String("events"), Key: map[string]types.AttributeValue{"id": s("x")}}); !errors.As(err, new(*types.ResourceNotFoundException)) {
		t.Fatalf("get on deleted table: %v", err)
	}
}

func TestDynamoDBConditionalWrites(t *testing.T) {
	ctx := context.Background()
	client := ddb(t)
	createOrders(t, client)

	item := map[string]types.AttributeValue{"customer": s("alice"), "order": n("1"), "total": n("10.50")}
	put := func() error {
		_, err := client.PutItem(ctx, &dynamodb.PutItemInput{
			TableName:                           aws.String("orders"),
			Item:                                item,
			ConditionExpression:                 aws.String("attribute_not_exists(customer)"),
			ReturnValuesOnConditionCheckFailure: types.ReturnValuesOnConditionCheckFailureAllOld,
		})
		return err
	}
	if err := put(); err != nil {
		t.Fatal(err)
	}
	var ccf *types.ConditionalCheckFailedException
	if err := put(); !errors.As(err, &ccf) {
		t.Fatalf("second put: %v", err)
	}
	if num(ccf.Item["total"]) != "10.5" {
		t.Fatalf("old item = %v", ccf.Item)
	}

	_, err := client.DeleteItem(ctx, &dynamodb.DeleteItemInput{
		TableName:                 aws.String("orders"),
		Key:                       map[string]types.AttributeValue{"customer": s("alice"), "order": n("1")},
		ConditionExpression:       aws.String("#t > :min"),
		ExpressionAttributeNames:  map[string]string{"#t": "total"},
		ExpressionAttributeValues: map[string]types.AttributeValue{":min": n("100")},
	})
	if !errors.As(err, &ccf) || ccf.Item != nil {
		t.Fatalf("conditional delete: %v", err)
	}
	deleted, err := client.DeleteItem(ctx, &dynamodb.DeleteItemInput{
		TableName:    aws.String("orders"),
		Key:          map[string]types.AttributeValue{"customer": s("alice"), "order": n("1")},
		ReturnValues: types.ReturnValueAllOld,
	})
	if err != nil || num(deleted.Attributes["total"]) != "10.5" {
		t.Fatalf("delete = %v, %v", deleted, err)
	}

	for name, in := range map[string]*dynamodb.PutItemInput{
		"missing key":    {TableName: aws.String("orders"), Item: map[string]types.AttributeValue{"customer": s("bob")}},
		"wrong key type": {TableName: aws.String("orders"), Item: map[string]types.AttributeValue{"customer": s("bob"), "order": s("1")}},
		"empty key":      {TableName: aws.String("orders"), Item: map[string]types.AttributeValue{"customer": s(""), "order": n("1")}},
		"unused value": {
			TableName: aws.String("orders"), Item: item,
			ConditionExpression:       aws.String("attribute_not_exists(customer)"),
			ExpressionAttributeValues: map[string]types.AttributeValue{":x": n("1")},
		},
		"undefined name": {TableName: aws.String("orders"), Item: item, ConditionExpression: aws.String("attribute_exists(#x)")},
		"reserved word":  {TableName: aws.String("orders"), Item: item, ConditionExpression: aws.String("attribute_exists(status)")},
		"bad number":     {TableName: aws.String("orders"), Item: map[string]types.AttributeValue{"customer": s("bob"), "order": n("1x")}},
	} {
		if _, err := client.PutItem(ctx, in); apiCode(err) != "ValidationException" {
			t.Errorf("%s: %v", name, err)
		}
	}
}

type order struct {
	Customer string            `dynamodbav:"customer"`
	Order    int               `dynamodbav:"order"`
	Count    int               `dynamodbav:"count"`
	Tags     []string          `dynamodbav:"tags,stringset"`
	Parts    []string          `dynamodbav:"parts"`
	Meta     map[string]string `dynamodbav:"meta"`
	Note     string            `dynamodbav:"note,omitempty"`
}

func TestDynamoDBUpdateExpressions(t *testing.T) {
	ctx := context.Background()
	client := ddb(t)
	createOrders(t, client)
	key := map[string]types.AttributeValue{"customer": s("carol"), "order": n("7")}

	update := func(expr string, values map[string]types.AttributeValue, rv types.ReturnValue) map[string]types.AttributeValue {
		t.Helper()
		out, err := client.UpdateItem(ctx, &dynamodb.UpdateItemInput{
			TableName:                 aws.String("orders"),
			Key:                       key,
			UpdateExpression:          aws.String(expr),
			ExpressionAttributeNames:  countName(expr),
			ExpressionAttributeValues: values,
			ReturnValues:              rv,
		})
		if err != nil {
			t.Fatalf("%s: %v", expr, err)
		}
		return out.Attributes
	}

	created := update("SET #c = if_not_exists(#c, :zero) + :one, parts = list_append(if_not_exists(parts, :empty), :l), meta = :m ADD tags :t",
		map[string]types.AttributeValue{
			":zero":  n("0"),
			":one":   n("1"),
			":empty": &types.AttributeValueMemberL{Value: []types.AttributeValue{}},
			":l":     &types.AttributeValueMemberL{Value: []types.AttributeValue{s("a"), s("b")}},
			":m":     &types.AttributeValueMemberM{Value: map[string]types.AttributeValue{"size": s("L")}},
			":t":     &types.AttributeValueMemberSS{Value: []string{"new"}},
		}, types.ReturnValueAllNew)
	if num(created["count"]) != "1" || str(created["customer"]) != "carol" {
		t.Fatalf("created = %v", created)
	}
	got := update("SET parts = list_append(parts, :more), meta.color = :red, note = :note ADD #c :two, tags :t2",
		map[string]types.AttributeValue{
			":more": &types.AttributeValueMemberL{Value: []types.AttributeValue{s("c"), s("d")}},
			":red":  s("red"), ":note": s("rush"), ":two": n("2"),
			":t2": &types.AttributeValueMemberSS{Value: []string{"vip"}},
		}, types.ReturnValueUpdatedNew)
	if num(got["count"]) != "3" || len(got) != 5 {
		t.Fatalf("updated new = %v", got)
	}
	old := update("REMOVE parts[0], parts[2], note DELETE tags :gone", map[string]types.AttributeValue{":gone": &types.AttributeValueMemberSS{Value: []string{"new"}}}, types.ReturnValueUpdatedOld)
	if str(old["note"]) != "rush" || len(old["parts"].(*types.AttributeValueMemberL).Value) != 2 || len(old["tags"].(*types.AttributeValueMemberSS).Value) != 2 {
		t.Fatalf("updated old = %v", old)
	}

	out, err := client.GetItem(ctx, &dynamodb.GetItemInput{TableName: aws.String("orders"), Key: key, ConsistentRead: aws.Bool(true)})
	if err != nil {
		t.Fatal(err)
	}
	var o order
	if err := attributevalue.UnmarshalMap(out.Item, &o); err != nil {
		t.Fatal(err)
	}
	slices.Sort(o.Tags)
	if o.Count != 3 || !slices.Equal(o.Parts, []string{"b", "d"}) || !slices.Equal(o.Tags, []string{"vip"}) ||
		o.Meta["color"] != "red" || o.Meta["size"] != "L" || o.Note != "" {
		t.Fatalf("item = %+v", o)
	}

	projected, err := client.GetItem(ctx, &dynamodb.GetItemInput{
		TableName:                aws.String("orders"),
		Key:                      key,
		ProjectionExpression:     aws.String("parts[1], #m.color"),
		ExpressionAttributeNames: map[string]string{"#m": "meta"},
	})
	if err != nil || len(projected.Item) != 2 || len(projected.Item["meta"].(*types.AttributeValueMemberM).Value) != 1 {
		t.Fatalf("projection = %v, %v", projected.Item, err)
	}

	for expr, values := range map[string]map[string]types.AttributeValue{
		"SET #c = #c + :s":                   {":s": s("x")},
		"SET customer = :s":                  {":s": s("x")},
		"SET a = :s REMOVE a":                {":s": s("x")},
		"SET nope.deep = :s":                 {":s": s("x")},
		"ADD parts :s":                       {":s": s("x")},
		"SET parts = list_append(parts, :s)": {":s": s("x")},
	} {
		_, err := client.UpdateItem(ctx, &dynamodb.UpdateItemInput{
			TableName: aws.String("orders"), Key: key, UpdateExpression: aws.String(expr),
			ExpressionAttributeValues: values, ExpressionAttributeNames: countName(expr),
		})
		if apiCode(err) != "ValidationException" {
			t.Errorf("%s: %v", expr, err)
		}
	}
}

func countName(expr string) map[string]string {
	if strings.Contains(expr, "#c") {
		return map[string]string{"#c": "count"}
	}
	return nil
}

func seedOrders(t testing.TB, client *dynamodb.Client, count int) {
	t.Helper()
	ctx := context.Background()
	var batch []types.WriteRequest
	flush := func() {
		if len(batch) == 0 {
			return
		}
		out, err := client.BatchWriteItem(ctx, &dynamodb.BatchWriteItemInput{RequestItems: map[string][]types.WriteRequest{"orders": batch}})
		if err != nil {
			t.Fatal(err)
		}
		if len(out.UnprocessedItems) != 0 {
			t.Fatalf("unprocessed = %v", out.UnprocessedItems)
		}
		batch = nil
	}
	for i := range count {
		status := "open"
		if i%3 == 0 {
			status = "shipped"
		}
		item, err := attributevalue.MarshalMap(map[string]any{
			"customer": fmt.Sprintf("c%d", i%4),
			"order":    i,
			"status":   status,
			"placedAt": fmt.Sprintf("2026-10-%02d", 1+i%28),
			"total":    i * 3,
		})
		if err != nil {
			t.Fatal(err)
		}
		batch = append(batch, types.WriteRequest{PutRequest: &types.PutRequest{Item: item}})
		if len(batch) == 25 {
			flush()
		}
	}
	flush()
}

func TestDynamoDBQueryGSIPagination(t *testing.T) {
	ctx := context.Background()
	client := ddb(t)
	createOrders(t, client)
	seedOrders(t, client, 60)

	var placed []string
	pages := 0
	p := dynamodb.NewQueryPaginator(client, &dynamodb.QueryInput{
		TableName:                 aws.String("orders"),
		IndexName:                 aws.String("by-status"),
		KeyConditionExpression:    aws.String("#s = :open AND placedAt BETWEEN :from AND :to"),
		ExpressionAttributeNames:  map[string]string{"#s": "status"},
		ExpressionAttributeValues: map[string]types.AttributeValue{":open": s("open"), ":from": s("2026-10-05"), ":to": s("2026-10-20")},
		Limit:                     aws.Int32(4),
	})
	for p.HasMorePages() {
		out, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		pages++
		for _, it := range out.Items {
			if _, ok := it["total"]; ok {
				t.Fatalf("KEYS_ONLY index returned a non-key attribute: %v", it)
			}
			placed = append(placed, str(it["placedAt"]))
		}
	}
	want := 0
	for i := range 60 {
		d := 1 + i%28
		if i%3 != 0 && d >= 5 && d <= 20 {
			want++
		}
	}
	if len(placed) != want || !slices.IsSorted(placed) || pages < want/4 {
		t.Fatalf("got %d items in %d pages (want %d): %v", len(placed), pages, want, placed)
	}

	desc, err := client.Query(ctx, &dynamodb.QueryInput{
		TableName:                 aws.String("orders"),
		KeyConditionExpression:    aws.String("customer = :c AND #o >= :min"),
		FilterExpression:          aws.String("#t > :t"),
		ExpressionAttributeNames:  map[string]string{"#o": "order", "#t": "total"},
		ExpressionAttributeValues: map[string]types.AttributeValue{":c": s("c1"), ":min": n("10"), ":t": n("100")},
		ScanIndexForward:          aws.Bool(false),
	})
	if err != nil {
		t.Fatal(err)
	}
	if desc.Count != 6 || desc.ScannedCount != 12 || num(desc.Items[0]["order"]) != "57" || num(desc.Items[5]["order"]) != "37" {
		t.Fatalf("query = count %d scanned %d first %v", desc.Count, desc.ScannedCount, desc.Items[0])
	}

	count, err := client.Query(ctx, &dynamodb.QueryInput{
		TableName:                 aws.String("orders"),
		KeyConditionExpression:    aws.String("customer = :c"),
		ExpressionAttributeValues: map[string]types.AttributeValue{":c": s("c2")},
		Select:                    types.SelectCount,
	})
	if err != nil || count.Count != 15 || count.Items != nil {
		t.Fatalf("count = %+v, %v", count, err)
	}

	_, err = client.Query(ctx, &dynamodb.QueryInput{
		TableName:                 aws.String("orders"),
		KeyConditionExpression:    aws.String("begins_with(customer, :c)"),
		ExpressionAttributeValues: map[string]types.AttributeValue{":c": s("c")},
	})
	if apiCode(err) != "ValidationException" {
		t.Fatalf("begins_with on hash key: %v", err)
	}
}

func TestDynamoDBScanFilter(t *testing.T) {
	ctx := context.Background()
	client := ddb(t)
	createOrders(t, client)
	seedOrders(t, client, 50)

	total := 0
	pages := 0
	for p := dynamodb.NewScanPaginator(client, &dynamodb.ScanInput{
		TableName:                 aws.String("orders"),
		FilterExpression:          aws.String("#s = :shipped AND #t BETWEEN :lo AND :hi"),
		ExpressionAttributeNames:  map[string]string{"#s": "status", "#t": "total"},
		ExpressionAttributeValues: map[string]types.AttributeValue{":shipped": s("shipped"), ":lo": n("30"), ":hi": n("120")},
		Limit:                     aws.Int32(7),
	}); p.HasMorePages(); {
		out, err := p.NextPage(ctx)
		if err != nil {
			t.Fatal(err)
		}
		pages++
		total += int(out.Count)
	}
	if total != 10 || pages != 8 {
		t.Fatalf("matched %d in %d pages", total, pages)
	}

	seen := 0
	for seg := range int32(3) {
		out, err := client.Scan(ctx, &dynamodb.ScanInput{TableName: aws.String("orders"), Segment: aws.Int32(seg), TotalSegments: aws.Int32(3)})
		if err != nil {
			t.Fatal(err)
		}
		seen += int(out.Count)
	}
	if seen != 50 {
		t.Fatalf("segments covered %d items", seen)
	}
}

func TestDynamoDBBatchWriteAndGet(t *testing.T) {
	ctx := context.Background()
	client := ddb(t)
	createOrders(t, client)
	seedOrders(t, client, 30)

	keys := make([]map[string]types.AttributeValue, 0, 10)
	for i := range 10 {
		keys = append(keys, map[string]types.AttributeValue{"customer": s(fmt.Sprintf("c%d", i%4)), "order": n(fmt.Sprint(i))})
	}
	keys = append(keys, map[string]types.AttributeValue{"customer": s("nobody"), "order": n("1")})
	out, err := client.BatchGetItem(ctx, &dynamodb.BatchGetItemInput{RequestItems: map[string]types.KeysAndAttributes{
		"orders": {Keys: keys, ProjectionExpression: aws.String("#o, #t"), ExpressionAttributeNames: map[string]string{"#o": "order", "#t": "total"}},
	}})
	if err != nil {
		t.Fatal(err)
	}
	if got := out.Responses["orders"]; len(got) != 10 || len(got[0]) != 2 || len(out.UnprocessedKeys) != 0 {
		t.Fatalf("responses = %v", got)
	}

	if _, err := client.BatchWriteItem(ctx, &dynamodb.BatchWriteItemInput{RequestItems: map[string][]types.WriteRequest{
		"orders": {
			{DeleteRequest: &types.DeleteRequest{Key: keys[0]}},
			{DeleteRequest: &types.DeleteRequest{Key: keys[0]}},
		},
	}}); apiCode(err) != "ValidationException" {
		t.Fatalf("duplicate keys: %v", err)
	}
	if _, err := client.BatchWriteItem(ctx, &dynamodb.BatchWriteItemInput{RequestItems: map[string][]types.WriteRequest{
		"orders": {{DeleteRequest: &types.DeleteRequest{Key: keys[0]}}, {DeleteRequest: &types.DeleteRequest{Key: keys[1]}}},
	}}); err != nil {
		t.Fatal(err)
	}
	scan, err := client.Scan(ctx, &dynamodb.ScanInput{TableName: aws.String("orders"), Select: types.SelectCount})
	if err != nil || scan.Count != 28 {
		t.Fatalf("scan after delete = %v, %v", scan, err)
	}
}

func TestDynamoDBTransactions(t *testing.T) {
	ctx := context.Background()
	client := ddb(t)
	createOrders(t, client)
	key := func(c, o string) map[string]types.AttributeValue {
		return map[string]types.AttributeValue{"customer": s(c), "order": n(o)}
	}
	if _, err := client.PutItem(ctx, &dynamodb.PutItemInput{TableName: aws.String("orders"), Item: map[string]types.AttributeValue{
		"customer": s("dave"), "order": n("1"), "stock": n("1"),
	}}); err != nil {
		t.Fatal(err)
	}

	_, err := client.TransactWriteItems(ctx, &dynamodb.TransactWriteItemsInput{TransactItems: []types.TransactWriteItem{
		{Put: &types.Put{TableName: aws.String("orders"), Item: map[string]types.AttributeValue{"customer": s("dave"), "order": n("2")}}},
		{Update: &types.Update{
			TableName: aws.String("orders"), Key: key("dave", "1"),
			UpdateExpression:                    aws.String("SET stock = stock - :one"),
			ConditionExpression:                 aws.String("stock >= :two"),
			ExpressionAttributeValues:           map[string]types.AttributeValue{":one": n("1"), ":two": n("2")},
			ReturnValuesOnConditionCheckFailure: types.ReturnValuesOnConditionCheckFailureAllOld,
		}},
	}})
	var tce *types.TransactionCanceledException
	if !errors.As(err, &tce) {
		t.Fatalf("transaction: %v", err)
	}
	if len(tce.CancellationReasons) != 2 || aws.ToString(tce.CancellationReasons[0].Code) != "None" ||
		aws.ToString(tce.CancellationReasons[1].Code) != "ConditionalCheckFailed" || num(tce.CancellationReasons[1].Item["stock"]) != "1" {
		t.Fatalf("reasons = %+v", tce.CancellationReasons)
	}
	if got, _ := client.GetItem(ctx, &dynamodb.GetItemInput{TableName: aws.String("orders"), Key: key("dave", "2")}); got.Item != nil {
		t.Fatalf("cancelled put was applied: %v", got.Item)
	}

	if _, err := client.TransactWriteItems(ctx, &dynamodb.TransactWriteItemsInput{
		ClientRequestToken: aws.String("order-2"),
		TransactItems: []types.TransactWriteItem{
			{Put: &types.Put{TableName: aws.String("orders"), Item: map[string]types.AttributeValue{"customer": s("dave"), "order": n("2")}}},
			{Update: &types.Update{
				TableName: aws.String("orders"), Key: key("dave", "1"),
				UpdateExpression:          aws.String("SET stock = stock - :one"),
				ConditionExpression:       aws.String("stock >= :one"),
				ExpressionAttributeValues: map[string]types.AttributeValue{":one": n("1")},
			}},
		},
	}); err != nil {
		t.Fatal(err)
	}
	got, err := client.TransactGetItems(ctx, &dynamodb.TransactGetItemsInput{TransactItems: []types.TransactGetItem{
		{Get: &types.Get{TableName: aws.String("orders"), Key: key("dave", "1")}},
		{Get: &types.Get{TableName: aws.String("orders"), Key: key("dave", "2")}},
		{Get: &types.Get{TableName: aws.String("orders"), Key: key("dave", "3")}},
	}})
	if err != nil || num(got.Responses[0].Item["stock"]) != "0" || got.Responses[1].Item == nil || got.Responses[2].Item != nil {
		t.Fatalf("transact get = %+v, %v", got, err)
	}

	if _, err := client.TransactWriteItems(ctx, &dynamodb.TransactWriteItemsInput{
		ClientRequestToken: aws.String("order-2"),
		TransactItems: []types.TransactWriteItem{
			{Delete: &types.Delete{TableName: aws.String("orders"), Key: key("dave", "2")}},
		},
	}); !errors.As(err, new(*types.IdempotentParameterMismatchException)) {
		t.Fatalf("reused token: %v", err)
	}
	if _, err := client.TransactWriteItems(ctx, &dynamodb.TransactWriteItemsInput{TransactItems: []types.TransactWriteItem{
		{Delete: &types.Delete{TableName: aws.String("orders"), Key: key("dave", "2")}},
		{ConditionCheck: &types.ConditionCheck{TableName: aws.String("orders"), Key: key("dave", "2"), ConditionExpression: aws.String("attribute_exists(customer)")}},
	}}); apiCode(err) != "ValidationException" {
		t.Fatalf("two operations on one item: %v", err)
	}
}

func TestDynamoDBMocksAreIsolated(t *testing.T) {
	ctx := context.Background()
	a, b := ddb(t), ddb(t)
	createOrders(t, a)
	if _, err := b.DescribeTable(ctx, &dynamodb.DescribeTableInput{TableName: aws.String("orders")}); !errors.As(err, new(*types.ResourceNotFoundException)) {
		t.Fatalf("table leaked into another mock: %v", err)
	}
	createOrders(t, b)
	if _, err := a.PutItem(ctx, &dynamodb.PutItemInput{TableName: aws.String("orders"), Item: map[string]types.AttributeValue{"customer": s("x"), "order": n("1")}}); err != nil {
		t.Fatal(err)
	}
	out, err := b.Scan(ctx, &dynamodb.ScanInput{TableName: aws.String("orders")})
	if err != nil || out.Count != 0 {
		t.Fatalf("item leaked into another mock: %v, %v", out, err)
	}
}

func TestDynamoDBPartiQL(t *testing.T) {
	ctx := context.Background()
	client := ddb(t)
	createOrders(t, client)
	if _, err := client.ExecuteStatement(ctx, &dynamodb.ExecuteStatementInput{
		Statement:  aws.String(`INSERT INTO "orders" VALUE {'customer': ?, 'order': 1, 'status': 'open'}`),
		Parameters: []types.AttributeValue{s("erin")},
	}); err != nil {
		t.Fatal(err)
	}
	out, err := client.ExecuteStatement(ctx, &dynamodb.ExecuteStatementInput{
		Statement: aws.String(`SELECT "order", status FROM orders WHERE customer = 'erin'`),
	})
	if err != nil || len(out.Items) != 1 || str(out.Items[0]["status"]) != "open" {
		t.Fatalf("select = %v, %v", out, err)
	}
}

func medianLatency(b *testing.B, op func() error) {
	b.Helper()
	samples := make([]time.Duration, 0, b.N)
	for b.Loop() {
		start := time.Now()
		if err := op(); err != nil {
			b.Fatal(err)
		}
		samples = append(samples, time.Since(start))
	}
	slices.Sort(samples)
	b.ReportMetric(float64(samples[len(samples)/2].Microseconds()), "median-µs")
}

func BenchmarkDynamoDB(b *testing.B) {
	ctx := context.Background()
	client := ddb(b)
	createOrders(b, client)
	seedOrders(b, client, 1000)
	i := 0
	b.Run("PutItem", func(b *testing.B) {
		medianLatency(b, func() error {
			i++
			_, err := client.PutItem(ctx, &dynamodb.PutItemInput{TableName: aws.String("orders"), Item: map[string]types.AttributeValue{
				"customer": s("bench"), "order": n(fmt.Sprint(i)), "status": s("open"), "placedAt": s("2026-10-28"),
			}})
			return err
		})
	})
	b.Run("GetItem", func(b *testing.B) {
		medianLatency(b, func() error {
			_, err := client.GetItem(ctx, &dynamodb.GetItemInput{TableName: aws.String("orders"), Key: map[string]types.AttributeValue{"customer": s("c1"), "order": n("5")}})
			return err
		})
	})
	b.Run("Query", func(b *testing.B) {
		medianLatency(b, func() error {
			_, err := client.Query(ctx, &dynamodb.QueryInput{
				TableName:                 aws.String("orders"),
				KeyConditionExpression:    aws.String("customer = :c AND #o < :n"),
				ExpressionAttributeNames:  map[string]string{"#o": "order"},
				ExpressionAttributeValues: map[string]types.AttributeValue{":c": s("c2"), ":n": n("100")},
			})
			return err
		})
	})
}

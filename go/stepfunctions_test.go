package wasmo_test

import (
	"context"
	"encoding/json"
	"slices"
	"sync/atomic"
	"testing"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/dynamodb"
	ddbtypes "github.com/aws/aws-sdk-go-v2/service/dynamodb/types"
	"github.com/aws/aws-sdk-go-v2/service/lambda"
	"github.com/aws/aws-sdk-go-v2/service/sfn"
	sfntypes "github.com/aws/aws-sdk-go-v2/service/sfn/types"
	"github.com/aws/aws-sdk-go-v2/service/sns"
	"github.com/aws/aws-sdk-go-v2/service/sqs"
)

const sfnRole = "arn:aws:iam::000000000000:role/sfn"

type TransientError struct{}

func (TransientError) Error() string { return "try again" }

type FatalError struct{}

func (FatalError) Error() string { return "cannot continue" }

func createMachine(t *testing.T, c *sfn.Client, name string, kind sfntypes.StateMachineType, definition any) string {
	t.Helper()
	def, err := json.Marshal(definition)
	if err != nil {
		t.Fatal(err)
	}
	out, err := c.CreateStateMachine(context.Background(), &sfn.CreateStateMachineInput{
		Name:       aws.String(name),
		Definition: aws.String(string(def)),
		RoleArn:    aws.String(sfnRole),
		Type:       kind,
	})
	if err != nil {
		t.Fatal(err)
	}
	return aws.ToString(out.StateMachineArn)
}

func startAndDescribe(t *testing.T, c *sfn.Client, machine, input string) *sfn.DescribeExecutionOutput {
	t.Helper()
	ctx := context.Background()
	started, err := c.StartExecution(ctx, &sfn.StartExecutionInput{StateMachineArn: aws.String(machine), Input: aws.String(input)})
	if err != nil {
		t.Fatal(err)
	}
	d, err := c.DescribeExecution(ctx, &sfn.DescribeExecutionInput{ExecutionArn: started.ExecutionArn})
	if err != nil {
		t.Fatal(err)
	}
	return d
}

func decode(t *testing.T, s *string) map[string]any {
	t.Helper()
	var v map[string]any
	if err := json.Unmarshal([]byte(aws.ToString(s)), &v); err != nil {
		t.Fatalf("decode %q: %v", aws.ToString(s), err)
	}
	return v
}

type obj = map[string]any

func TestStepFunctionsLambdaRetryCatchDynamoDBAndSNS(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	cfg := m.Config()
	lc := lambda.NewFromConfig(cfg)
	sc := sfn.NewFromConfig(cfg)
	ddb := dynamodb.NewFromConfig(cfg)
	snsc := sns.NewFromConfig(cfg)
	sqsc := sqs.NewFromConfig(cfg)

	priceArn := createFunction(t, lc, "price", nil)
	createFunction(t, lc, "charge", nil)
	var priceCalls, chargeCalls atomic.Int32
	m.RegisterFunction("price", func(_ context.Context, event json.RawMessage) (json.RawMessage, error) {
		if priceCalls.Add(1) == 1 {
			return nil, TransientError{}
		}
		var in struct {
			ID  string
			Qty int
		}
		if err := json.Unmarshal(event, &in); err != nil {
			return nil, err
		}
		return json.Marshal(obj{"total": in.Qty * 25})
	})
	m.RegisterFunction("charge", func(context.Context, json.RawMessage) (json.RawMessage, error) {
		chargeCalls.Add(1)
		return nil, FatalError{}
	})

	if _, err := ddb.CreateTable(ctx, &dynamodb.CreateTableInput{
		TableName:            aws.String("orders"),
		AttributeDefinitions: []ddbtypes.AttributeDefinition{{AttributeName: aws.String("id"), AttributeType: ddbtypes.ScalarAttributeTypeS}},
		KeySchema:            []ddbtypes.KeySchemaElement{{AttributeName: aws.String("id"), KeyType: ddbtypes.KeyTypeHash}},
		BillingMode:          ddbtypes.BillingModePayPerRequest,
	}); err != nil {
		t.Fatal(err)
	}
	topic, err := snsc.CreateTopic(ctx, &sns.CreateTopicInput{Name: aws.String("orders")})
	if err != nil {
		t.Fatal(err)
	}
	queueURL, queueArn := queueWithArn(t, sqsc, "orders", nil)
	if _, err := snsc.Subscribe(ctx, &sns.SubscribeInput{
		TopicArn:   topic.TopicArn,
		Protocol:   aws.String("sqs"),
		Endpoint:   aws.String(queueArn),
		Attributes: map[string]string{"RawMessageDelivery": "true"},
	}); err != nil {
		t.Fatal(err)
	}

	machine := createMachine(t, sc, "orders", sfntypes.StateMachineTypeStandard, obj{
		"StartAt": "Price",
		"States": obj{
			"Price": obj{
				"Type":           "Task",
				"Resource":       "arn:aws:states:::lambda:invoke",
				"Parameters":     obj{"FunctionName": priceArn, "Payload.$": "$"},
				"ResultSelector": obj{"total.$": "$.Payload.total"},
				"ResultPath":     "$.price",
				"Retry":          []obj{{"ErrorEquals": []string{"TransientError"}, "IntervalSeconds": 1, "MaxAttempts": 2}},
				"Next":           "Charge",
			},
			"Charge": obj{
				"Type":     "Task",
				"Resource": "arn:aws:lambda:us-east-1:000000000000:function:charge",
				"Catch":    []obj{{"ErrorEquals": []string{"FatalError"}, "ResultPath": "$.chargeError", "Next": "Save"}},
				"Next":     "Save",
			},
			"Save": obj{
				"Type":     "Task",
				"Resource": "arn:aws:states:::dynamodb:putItem",
				"Parameters": obj{
					"TableName": "orders",
					"Item": obj{
						"id":    obj{"S.$": "$.ID"},
						"total": obj{"N.$": "States.Format('{}', $.price.total)"},
					},
				},
				"ResultPath": nil,
				"Next":       "Notify",
			},
			"Notify": obj{
				"Type":       "Task",
				"Resource":   "arn:aws:states:::sns:publish",
				"Parameters": obj{"TopicArn": aws.ToString(topic.TopicArn), "Message.$": "$.price"},
				"ResultPath": nil,
				"End":        true,
			},
		},
	})

	d := startAndDescribe(t, sc, machine, `{"ID":"o-1","Qty":4}`)
	if d.Status != sfntypes.ExecutionStatusSucceeded {
		t.Fatalf("status = %s %s %s", d.Status, aws.ToString(d.Error), aws.ToString(d.Cause))
	}
	out := decode(t, d.Output)
	if out["price"].(obj)["total"] != 100.0 {
		t.Fatalf("output = %v", out)
	}
	if out["chargeError"].(obj)["Error"] != "FatalError" {
		t.Fatalf("chargeError = %v", out["chargeError"])
	}
	if priceCalls.Load() != 2 || chargeCalls.Load() != 1 {
		t.Fatalf("calls = %d price, %d charge", priceCalls.Load(), chargeCalls.Load())
	}

	item, err := ddb.GetItem(ctx, &dynamodb.GetItemInput{
		TableName: aws.String("orders"),
		Key:       map[string]ddbtypes.AttributeValue{"id": &ddbtypes.AttributeValueMemberS{Value: "o-1"}},
	})
	if err != nil {
		t.Fatal(err)
	}
	if n, ok := item.Item["total"].(*ddbtypes.AttributeValueMemberN); !ok || n.Value != "100" {
		t.Fatalf("item = %#v", item.Item)
	}

	msgs := drain(t, sqsc, queueURL)
	if len(msgs) != 1 || aws.ToString(msgs[0].Body) != `{"total":100}` {
		t.Fatalf("messages = %+v", msgs)
	}

	hist, err := sc.GetExecutionHistory(ctx, &sfn.GetExecutionHistoryInput{ExecutionArn: d.ExecutionArn})
	if err != nil {
		t.Fatal(err)
	}
	var kinds []sfntypes.HistoryEventType
	for _, e := range hist.Events {
		kinds = append(kinds, e.Type)
	}
	for _, want := range []sfntypes.HistoryEventType{
		sfntypes.HistoryEventTypeExecutionStarted,
		sfntypes.HistoryEventTypeTaskFailed,
		sfntypes.HistoryEventTypeTaskSucceeded,
		sfntypes.HistoryEventTypeLambdaFunctionFailed,
		sfntypes.HistoryEventTypeExecutionSucceeded,
	} {
		if !slices.Contains(kinds, want) {
			t.Fatalf("history %v lacks %s", kinds, want)
		}
	}
}

func TestStepFunctionsStartSyncExecutionMapAndParallel(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	cfg := m.Config()
	lc := lambda.NewFromConfig(cfg)
	sc := sfn.NewFromConfig(cfg)
	square := createFunction(t, lc, "square", nil)
	var calls atomic.Int32
	m.RegisterFunction("square", func(_ context.Context, event json.RawMessage) (json.RawMessage, error) {
		calls.Add(1)
		var n int
		if err := json.Unmarshal(event, &n); err != nil {
			return nil, err
		}
		return json.Marshal(n * n)
	})
	machine := createMachine(t, sc, "squares", sfntypes.StateMachineTypeExpress, obj{
		"StartAt": "Both",
		"States": obj{
			"Both": obj{
				"Type": "Parallel",
				"Branches": []obj{
					{"StartAt": "Squares", "States": obj{"Squares": obj{
						"Type":           "Map",
						"ItemsPath":      "$.xs",
						"MaxConcurrency": 2,
						"ItemProcessor": obj{
							"StartAt": "Square",
							"States":  obj{"Square": obj{"Type": "Task", "Resource": square, "End": true}},
						},
						"End": true,
					}}},
					{"StartAt": "Count", "States": obj{"Count": obj{
						"Type":       "Pass",
						"Parameters": obj{"n.$": "States.ArrayLength($.xs)"},
						"End":        true,
					}}},
				},
				"End": true,
			},
		},
	})
	out, err := sc.StartSyncExecution(ctx, &sfn.StartSyncExecutionInput{
		StateMachineArn: aws.String(machine),
		Input:           aws.String(`{"xs":[1,2,3,4]}`),
	})
	if err != nil {
		t.Fatal(err)
	}
	if out.Status != sfntypes.SyncExecutionStatusSucceeded || aws.ToString(out.Output) != `[[1,4,9,16],{"n":4}]` {
		t.Fatalf("sync = %s %s %s", out.Status, aws.ToString(out.Output), aws.ToString(out.Cause))
	}
	if calls.Load() != 4 {
		t.Fatalf("calls = %d", calls.Load())
	}
}

func TestStepFunctionsHandlerSendsTaskSuccess(t *testing.T) {
	m := newMock(t)
	cfg := m.Config()
	lc := lambda.NewFromConfig(cfg)
	sc := sfn.NewFromConfig(cfg)
	createFunction(t, lc, "approver", nil)
	m.RegisterFunction("approver", func(ctx context.Context, event json.RawMessage) (json.RawMessage, error) {
		var in struct{ Token, Who string }
		if err := json.Unmarshal(event, &in); err != nil {
			return nil, err
		}
		_, err := sc.SendTaskSuccess(ctx, &sfn.SendTaskSuccessInput{
			TaskToken: aws.String(in.Token),
			Output:    aws.String(`{"approvedBy":"` + in.Who + `"}`),
		})
		return json.RawMessage(`null`), err
	})
	machine := createMachine(t, sc, "approval", sfntypes.StateMachineTypeStandard, obj{
		"StartAt": "Approve",
		"States": obj{"Approve": obj{
			"Type":     "Task",
			"Resource": "arn:aws:states:::lambda:invoke.waitForTaskToken",
			"Parameters": obj{
				"FunctionName": "approver",
				"Payload":      obj{"Token.$": "$$.Task.Token", "Who.$": "$.who"},
			},
			"ResultPath": "$.approval",
			"End":        true,
		}},
	})
	d := startAndDescribe(t, sc, machine, `{"who":"kim"}`)
	if d.Status != sfntypes.ExecutionStatusSucceeded || aws.ToString(d.Output) != `{"who":"kim","approval":{"approvedBy":"kim"}}` {
		t.Fatalf("execution = %s %s %s", d.Status, aws.ToString(d.Output), aws.ToString(d.Cause))
	}
}

func TestStepFunctionsActivityFailure(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	sc := sfn.NewFromConfig(m.Config())
	activity, err := sc.CreateActivity(ctx, &sfn.CreateActivityInput{Name: aws.String("review")})
	if err != nil {
		t.Fatal(err)
	}
	machine := createMachine(t, sc, "review", sfntypes.StateMachineTypeStandard, obj{
		"StartAt": "Review",
		"States":  obj{"Review": obj{"Type": "Task", "Resource": aws.ToString(activity.ActivityArn), "End": true}},
	})
	started, err := sc.StartExecution(ctx, &sfn.StartExecutionInput{StateMachineArn: aws.String(machine), Input: aws.String(`{"doc":7}`)})
	if err != nil {
		t.Fatal(err)
	}
	task, err := sc.GetActivityTask(ctx, &sfn.GetActivityTaskInput{ActivityArn: activity.ActivityArn, WorkerName: aws.String("w1")})
	if err != nil {
		t.Fatal(err)
	}
	if aws.ToString(task.Input) != `{"doc":7}` || aws.ToString(task.TaskToken) == "" {
		t.Fatalf("task = %+v", task)
	}
	if _, err := sc.SendTaskFailure(ctx, &sfn.SendTaskFailureInput{
		TaskToken: task.TaskToken,
		Error:     aws.String("Rejected"),
		Cause:     aws.String("needs work"),
	}); err != nil {
		t.Fatal(err)
	}
	d, err := sc.DescribeExecution(ctx, &sfn.DescribeExecutionInput{ExecutionArn: started.ExecutionArn})
	if err != nil {
		t.Fatal(err)
	}
	if d.Status != sfntypes.ExecutionStatusFailed || aws.ToString(d.Error) != "Rejected" || aws.ToString(d.Cause) != "needs work" {
		t.Fatalf("execution = %s %s %s", d.Status, aws.ToString(d.Error), aws.ToString(d.Cause))
	}
	_, err = sc.SendTaskSuccess(ctx, &sfn.SendTaskSuccessInput{TaskToken: task.TaskToken, Output: aws.String("{}")})
	if apiCode(err) != "TaskDoesNotExist" {
		t.Fatalf("reused token: %v", err)
	}
}

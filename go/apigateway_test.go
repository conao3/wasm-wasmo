package wasmo_test

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"strings"
	"testing"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/apigateway"
	"github.com/aws/aws-sdk-go-v2/service/lambda"
	"github.com/aws/aws-sdk-go-v2/service/sqs"
	wasmo "github.com/conao3/wasm-wasmo/go"
)

func lambdaURI(arn string) string {
	return "arn:aws:apigateway:us-east-1:lambda:path/2015-03-31/functions/" + arn + "/invocations"
}

// call sends an unsigned HTTP request to the execute-api endpoint through the mock.
func call(t *testing.T, m *wasmo.Mock, method, url string, headers map[string]string, body string) (int, http.Header, string) {
	t.Helper()
	req, err := http.NewRequest(method, url, strings.NewReader(body))
	if err != nil {
		t.Fatal(err)
	}
	for k, v := range headers {
		req.Header.Set(k, v)
	}
	res, err := m.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	b, err := io.ReadAll(res.Body)
	if err != nil {
		t.Fatal(err)
	}
	return res.StatusCode, res.Header, string(b)
}

type restAPI struct {
	t   *testing.T
	c   *apigateway.Client
	id  string
	ctx context.Context
}

func (a restAPI) resource(parent, part string) string {
	out, err := a.c.CreateResource(a.ctx, &apigateway.CreateResourceInput{RestApiId: aws.String(a.id), ParentId: aws.String(parent), PathPart: aws.String(part)})
	if err != nil {
		a.t.Fatal(err)
	}
	return aws.ToString(out.Id)
}

func (a restAPI) method(res, verb string, in apigateway.PutMethodInput, integration apigateway.PutIntegrationInput) {
	in.RestApiId, in.ResourceId, in.HttpMethod = aws.String(a.id), aws.String(res), aws.String(verb)
	if in.AuthorizationType == nil {
		in.AuthorizationType = aws.String("NONE")
	}
	if _, err := a.c.PutMethod(a.ctx, &in); err != nil {
		a.t.Fatal(err)
	}
	integration.RestApiId, integration.ResourceId, integration.HttpMethod = aws.String(a.id), aws.String(res), aws.String(verb)
	if _, err := a.c.PutIntegration(a.ctx, &integration); err != nil {
		a.t.Fatal(err)
	}
}

func (a restAPI) response(res, verb, status string, in apigateway.PutIntegrationResponseInput) {
	in.RestApiId, in.ResourceId, in.HttpMethod, in.StatusCode = aws.String(a.id), aws.String(res), aws.String(verb), aws.String(status)
	if _, err := a.c.PutIntegrationResponse(a.ctx, &in); err != nil {
		a.t.Fatal(err)
	}
}

func TestAPIGatewayRestAPI(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	lc := lambda.NewFromConfig(m.Config())
	gw := apigateway.NewFromConfig(m.Config())
	sc := sqs.NewFromConfig(m.Config())

	echo := createFunction(t, lc, "echo", nil)
	auth := createFunction(t, lc, "auth", nil)
	m.RegisterFunction("echo", func(_ context.Context, event json.RawMessage) (json.RawMessage, error) {
		var e struct {
			Resource       string            `json:"resource"`
			Path           string            `json:"path"`
			PathParameters map[string]string `json:"pathParameters"`
			Query          map[string]string `json:"queryStringParameters"`
			StageVariables map[string]string `json:"stageVariables"`
			Context        struct {
				Stage      string         `json:"stage"`
				Authorizer map[string]any `json:"authorizer"`
			} `json:"requestContext"`
		}
		if err := json.Unmarshal(event, &e); err != nil {
			return nil, err
		}
		body, _ := json.Marshal(map[string]any{
			"resource": e.Resource, "path": e.Path, "params": e.PathParameters, "query": e.Query,
			"vars": e.StageVariables, "stage": e.Context.Stage, "principal": e.Context.Authorizer["principalId"],
		})
		return json.Marshal(map[string]any{"statusCode": 202, "headers": map[string]string{"X-Echo": "yes"}, "body": string(body)})
	})
	m.RegisterFunction("auth", func(_ context.Context, event json.RawMessage) (json.RawMessage, error) {
		var e struct {
			Token     string `json:"authorizationToken"`
			MethodArn string `json:"methodArn"`
		}
		if err := json.Unmarshal(event, &e); err != nil {
			return nil, err
		}
		effect := "Deny"
		if e.Token == "allow" {
			effect = "Allow"
		}
		return json.Marshal(map[string]any{
			"principalId": "user-1",
			"policyDocument": map[string]any{"Version": "2012-10-17", "Statement": []map[string]any{
				{"Action": "execute-api:Invoke", "Effect": effect, "Resource": e.MethodArn},
			}},
		})
	})
	queue, err := sc.CreateQueue(ctx, &sqs.CreateQueueInput{QueueName: aws.String("orders")})
	if err != nil {
		t.Fatal(err)
	}

	created, err := gw.CreateRestApi(ctx, &apigateway.CreateRestApiInput{Name: aws.String("shop")})
	if err != nil {
		t.Fatal(err)
	}
	a := restAPI{t: t, c: gw, id: aws.ToString(created.Id), ctx: ctx}
	root := aws.ToString(created.RootResourceId)
	proxyIntegration := apigateway.PutIntegrationInput{Type: "AWS_PROXY", IntegrationHttpMethod: aws.String("POST"), Uri: aws.String(lambdaURI(echo))}

	pets := a.resource(root, "pets")
	pet := a.resource(pets, "{id}")
	a.method(pet, "GET", apigateway.PutMethodInput{}, proxyIntegration)
	greedy := a.resource(a.resource(root, "files"), "{proxy+}")
	a.method(greedy, "ANY", apigateway.PutMethodInput{}, proxyIntegration)

	a.method(pets, "POST", apigateway.PutMethodInput{}, apigateway.PutIntegrationInput{
		Type:             "MOCK",
		RequestTemplates: map[string]string{"application/json": `{"statusCode": 201}`},
	})
	a.response(pets, "POST", "201", apigateway.PutIntegrationResponseInput{
		SelectionPattern:  aws.String("201"),
		ResponseTemplates: map[string]string{"application/json": `{"name": "$input.params('x-name')", "stage": "$context.stage", "env": "$stageVariables.env"}`},
	})

	orders := a.resource(root, "orders")
	a.method(orders, "POST", apigateway.PutMethodInput{}, apigateway.PutIntegrationInput{
		Type:                  "AWS",
		IntegrationHttpMethod: aws.String("POST"),
		Uri:                   aws.String("arn:aws:apigateway:us-east-1:sqs:path/000000000000/orders"),
		RequestTemplates:      map[string]string{"application/json": "Action=SendMessage&MessageBody=$util.urlEncode($input.body)"},
	})
	a.response(orders, "POST", "200", apigateway.PutIntegrationResponseInput{
		ResponseTemplates: map[string]string{"application/json": `{"id": "$input.path('$.MessageId')"}`},
	})

	authorizer, err := gw.CreateAuthorizer(ctx, &apigateway.CreateAuthorizerInput{
		RestApiId: aws.String(a.id), Name: aws.String("token"), Type: "TOKEN",
		AuthorizerUri: aws.String(lambdaURI(auth)), IdentitySource: aws.String("method.request.header.Authorization"),
	})
	if err != nil {
		t.Fatal(err)
	}
	secure := a.resource(root, "secure")
	a.method(secure, "GET", apigateway.PutMethodInput{AuthorizationType: aws.String("CUSTOM"), AuthorizerId: authorizer.Id}, proxyIntegration)

	if _, err := gw.CreateDeployment(ctx, &apigateway.CreateDeploymentInput{
		RestApiId: aws.String(a.id), StageName: aws.String("prod"), Variables: map[string]string{"env": "production"},
	}); err != nil {
		t.Fatal(err)
	}
	base := "https://" + a.id + ".execute-api.us-east-1.amazonaws.com/prod"

	status, header, body := call(t, m, "GET", base+"/pets/7?color=red", nil, "")
	if status != 202 || header.Get("X-Echo") != "yes" {
		t.Fatalf("GET /pets/7: %d %v %s", status, header, body)
	}
	var got map[string]any
	if err := json.Unmarshal([]byte(body), &got); err != nil {
		t.Fatal(err)
	}
	want := map[string]any{
		"resource": "/pets/{id}", "path": "/pets/7", "params": map[string]any{"id": "7"}, "query": map[string]any{"color": "red"},
		"vars": map[string]any{"env": "production"}, "stage": "prod", "principal": nil,
	}
	if g, w := mustJSON(t, got), mustJSON(t, want); g != w {
		t.Fatalf("proxy event = %s, want %s", g, w)
	}

	_, _, body = call(t, m, "DELETE", base+"/files/a/b.txt", nil, "")
	if !strings.Contains(body, `"params":{"proxy":"a/b.txt"}`) {
		t.Fatalf("greedy path: %s", body)
	}

	status, _, body = call(t, m, "POST", base+"/pets", map[string]string{"Content-Type": "application/json", "X-Name": "rex"}, `{}`)
	if status != 201 || body != `{"name": "rex", "stage": "prod", "env": "production"}` {
		t.Fatalf("MOCK: %d %s", status, body)
	}

	status, _, body = call(t, m, "POST", base+"/orders", map[string]string{"Content-Type": "application/json"}, `{"sku":"a1"}`)
	if status != 200 || !strings.HasPrefix(body, `{"id": "`) || body == `{"id": ""}` {
		t.Fatalf("SQS integration: %d %s", status, body)
	}
	msgs, err := sc.ReceiveMessage(ctx, &sqs.ReceiveMessageInput{QueueUrl: queue.QueueUrl})
	if err != nil {
		t.Fatal(err)
	}
	if len(msgs.Messages) != 1 || aws.ToString(msgs.Messages[0].Body) != `{"sku":"a1"}` {
		t.Fatalf("queued: %+v", msgs.Messages)
	}

	if status, _, _ := call(t, m, "GET", base+"/secure", nil, ""); status != 401 {
		t.Fatalf("no token: %d", status)
	}
	if status, _, _ := call(t, m, "GET", base+"/secure", map[string]string{"Authorization": "deny"}, ""); status != 403 {
		t.Fatalf("deny: %d", status)
	}
	status, _, body = call(t, m, "GET", base+"/secure", map[string]string{"Authorization": "allow"}, "")
	if status != 202 || !strings.Contains(body, `"principal":"user-1"`) {
		t.Fatalf("allow: %d %s", status, body)
	}

	status, _, body = call(t, m, "GET", base+"/nothing", nil, "")
	if status != 403 || body != `{"message":"Missing Authentication Token"}` {
		t.Fatalf("unknown route: %d %s", status, body)
	}
}

func mustJSON(t *testing.T, v any) string {
	t.Helper()
	b, err := json.Marshal(v)
	if err != nil {
		t.Fatal(err)
	}
	return string(b)
}

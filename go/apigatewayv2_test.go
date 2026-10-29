package wasmo_test

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"testing"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/service/apigatewayv2"
	v2types "github.com/aws/aws-sdk-go-v2/service/apigatewayv2/types"
	"github.com/aws/aws-sdk-go-v2/service/lambda"
	"github.com/aws/aws-sdk-go-v2/service/sfn"
	sfntypes "github.com/aws/aws-sdk-go-v2/service/sfn/types"
)

func unsignedJWT(claims obj) string {
	enc := base64.RawURLEncoding
	body, _ := json.Marshal(claims)
	return enc.EncodeToString([]byte(`{"alg":"none"}`)) + "." + enc.EncodeToString(body) + ".sig"
}

func TestAPIGatewayV2HTTPAPI(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	cfg := m.Config()
	lc := lambda.NewFromConfig(cfg)
	sc := sfn.NewFromConfig(cfg)
	gc := apigatewayv2.NewFromConfig(cfg)

	pets := createFunction(t, lc, "pets", nil)
	m.RegisterFunction("pets", func(_ context.Context, event json.RawMessage) (json.RawMessage, error) {
		var e obj
		if err := json.Unmarshal(event, &e); err != nil {
			return nil, err
		}
		rc, _ := e["requestContext"].(obj)
		body, _ := json.Marshal(obj{
			"version":    e["version"],
			"params":     e["pathParameters"],
			"vars":       e["stageVariables"],
			"authorizer": rc["authorizer"],
			"path":       e["path"],
		})
		return json.Marshal(obj{"statusCode": 200, "headers": obj{"x-handled": "pets"}, "body": string(body)})
	})
	double := createFunction(t, lc, "double", nil)
	m.RegisterFunction("double", func(_ context.Context, event json.RawMessage) (json.RawMessage, error) {
		var e struct{ N int }
		if err := json.Unmarshal(event, &e); err != nil {
			return nil, err
		}
		return json.Marshal(obj{"n": e.N * 2})
	})
	machine := createMachine(t, sc, "doubler", sfntypes.StateMachineTypeExpress, obj{
		"StartAt": "D",
		"States":  obj{"D": obj{"Type": "Task", "Resource": double, "End": true}},
	})

	api, err := gc.CreateApi(ctx, &apigatewayv2.CreateApiInput{Name: aws.String("shop"), ProtocolType: v2types.ProtocolTypeHttp})
	if err != nil {
		t.Fatal(err)
	}
	id := aws.ToString(api.ApiId)
	integration := func(in apigatewayv2.CreateIntegrationInput) string {
		in.ApiId = aws.String(id)
		out, err := gc.CreateIntegration(ctx, &in)
		if err != nil {
			t.Fatal(err)
		}
		return "integrations/" + aws.ToString(out.IntegrationId)
	}
	v2 := integration(apigatewayv2.CreateIntegrationInput{IntegrationType: v2types.IntegrationTypeAwsProxy, IntegrationUri: aws.String(pets), PayloadFormatVersion: aws.String("2.0")})
	v1 := integration(apigatewayv2.CreateIntegrationInput{IntegrationType: v2types.IntegrationTypeAwsProxy, IntegrationUri: aws.String(pets), PayloadFormatVersion: aws.String("1.0")})
	sync := integration(apigatewayv2.CreateIntegrationInput{
		IntegrationType:      v2types.IntegrationTypeAwsProxy,
		IntegrationSubtype:   aws.String("StepFunctions-StartSyncExecution"),
		PayloadFormatVersion: aws.String("1.0"),
		CredentialsArn:       aws.String("arn:aws:iam::000000000000:role/apigw"),
		RequestParameters:    map[string]string{"StateMachineArn": machine, "Input": "$request.body"},
	})
	auth, err := gc.CreateAuthorizer(ctx, &apigatewayv2.CreateAuthorizerInput{
		ApiId:            aws.String(id),
		Name:             aws.String("jwt"),
		AuthorizerType:   v2types.AuthorizerTypeJwt,
		IdentitySource:   []string{"$request.header.Authorization"},
		JwtConfiguration: &v2types.JWTConfiguration{Issuer: aws.String("https://issuer.example"), Audience: []string{"app"}},
	})
	if err != nil {
		t.Fatal(err)
	}
	for _, r := range []apigatewayv2.CreateRouteInput{
		{RouteKey: aws.String("GET /pets/{id}"), Target: aws.String(v2)},
		{RouteKey: aws.String("ANY /{proxy+}"), Target: aws.String(v1)},
		{RouteKey: aws.String("POST /double"), Target: aws.String(sync)},
		{RouteKey: aws.String("GET /me"), Target: aws.String(v2), AuthorizationType: v2types.AuthorizationTypeJwt, AuthorizerId: auth.AuthorizerId},
	} {
		r.ApiId = aws.String(id)
		if _, err := gc.CreateRoute(ctx, &r); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := gc.CreateStage(ctx, &apigatewayv2.CreateStageInput{
		ApiId: aws.String(id), StageName: aws.String("prod"), AutoDeploy: aws.Bool(true), StageVariables: map[string]string{"env": "p"},
	}); err != nil {
		t.Fatal(err)
	}
	host := "https://" + id + ".execute-api.us-east-1.amazonaws.com/prod"

	decode := func(s string) obj {
		var o obj
		if err := json.Unmarshal([]byte(s), &o); err != nil {
			t.Fatalf("%s: %v", s, err)
		}
		return o
	}
	status, h, body := call(t, m, "GET", host+"/pets/7", nil, "")
	if status != 200 || h.Get("x-handled") != "pets" {
		t.Fatalf("GET /pets/7: %d %v %s", status, h, body)
	}
	b := decode(body)
	if b["version"] != "2.0" || b["params"].(obj)["id"] != "7" || b["vars"].(obj)["env"] != "p" {
		t.Fatalf("payload 2.0: %s", body)
	}
	status, _, body = call(t, m, "DELETE", host+"/files/a/b", nil, "")
	b = decode(body)
	if status != 200 || b["version"] != "1.0" || b["params"].(obj)["proxy"] != "files/a/b" || b["path"] != "/files/a/b" {
		t.Fatalf("payload 1.0: %d %s", status, body)
	}

	if status, _, body := call(t, m, "GET", host+"/me", nil, ""); status != 401 {
		t.Fatalf("no token: %d %s", status, body)
	}
	token := unsignedJWT(obj{"iss": "https://issuer.example", "aud": "app", "sub": "s1"})
	status, _, body = call(t, m, "GET", host+"/me", map[string]string{"Authorization": "Bearer " + token}, "")
	if status != 200 {
		t.Fatalf("token: %d %s", status, body)
	}
	claims := decode(body)["authorizer"].(obj)["jwt"].(obj)["claims"].(obj)
	if claims["sub"] != "s1" {
		t.Fatalf("claims: %v", claims)
	}

	status, _, body = call(t, m, "POST", host+"/double", map[string]string{"Content-Type": "application/json"}, `{"N":21}`)
	if status != 200 {
		t.Fatalf("sync execution: %d %s", status, body)
	}
	out := decode(body)
	if out["status"] != "SUCCEEDED" || out["output"] != `{"n":42}` {
		t.Fatalf("sync output: %s", body)
	}
}

func TestAPIGatewayV2QuickCreate(t *testing.T) {
	ctx := context.Background()
	m := newMock(t)
	cfg := m.Config()
	lc := lambda.NewFromConfig(cfg)
	gc := apigatewayv2.NewFromConfig(cfg)
	fn := createFunction(t, lc, "hello", nil)
	m.RegisterFunction("hello", func(_ context.Context, event json.RawMessage) (json.RawMessage, error) {
		var e obj
		if err := json.Unmarshal(event, &e); err != nil {
			return nil, err
		}
		return json.Marshal(obj{"routeKey": e["routeKey"], "rawPath": e["rawPath"], "stage": e["requestContext"].(obj)["stage"]})
	})
	api, err := gc.CreateApi(ctx, &apigatewayv2.CreateApiInput{Name: aws.String("quick"), ProtocolType: v2types.ProtocolTypeHttp, Target: aws.String(fn)})
	if err != nil {
		t.Fatal(err)
	}
	status, h, body := call(t, m, "GET", aws.ToString(api.ApiEndpoint)+"/any/where?x=1", nil, "")
	if status != 200 || h.Get("Content-Type") != "application/json" {
		t.Fatalf("quick create: %d %v %s", status, h, body)
	}
	var got obj
	if err := json.Unmarshal([]byte(body), &got); err != nil {
		t.Fatal(err)
	}
	if got["routeKey"] != "$default" || got["rawPath"] != "/any/where" || got["stage"] != "$default" {
		t.Fatalf("quick create payload: %s", body)
	}
}

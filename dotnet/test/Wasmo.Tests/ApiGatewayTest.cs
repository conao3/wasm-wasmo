using System.Net;
using System.Text;
using System.Text.Json;
using Amazon.APIGateway;
using Amazon.APIGateway.Model;
using Amazon.Lambda;
using Amazon.Lambda.Model;

namespace Wasmo.Tests;

public class ApiGatewayTest
{
    [Fact]
    public async Task RestApiInvokesHandlerThroughExecuteApi()
    {
        using var mock = WasmoMock.Create();
        using var lambda = new AmazonLambdaClient(mock.Configure(new AmazonLambdaConfig()));
        using var apigw = new AmazonAPIGatewayClient(mock.Configure(new AmazonAPIGatewayConfig()));
        var fn = await lambda.CreateFunctionAsync(new CreateFunctionRequest
        {
            FunctionName = "pets",
            Runtime = Runtime.Dotnet8,
            Role = "arn:aws:iam::000000000000:role/lambda",
            Handler = "App::App.Function::Handler",
            Code = new FunctionCode { ZipFile = new MemoryStream(Encoding.UTF8.GetBytes("PK fake zip")) },
        });
        mock.RegisterFunction("pets", (eventJson, _) =>
        {
            var e = JsonDocument.Parse(eventJson).RootElement;
            var id = e.GetProperty("pathParameters").GetProperty("id").GetString();
            var env = e.GetProperty("stageVariables").GetProperty("env").GetString();
            return JsonSerializer.Serialize(new
            {
                statusCode = 200,
                headers = new Dictionary<string, string> { ["x-pet"] = id! },
                body = JsonSerializer.Serialize(new { id, env }),
            });
        });
        var api = await apigw.CreateRestApiAsync(new CreateRestApiRequest { Name = "shop" });
        var pets = await apigw.CreateResourceAsync(new CreateResourceRequest
        {
            RestApiId = api.Id,
            ParentId = api.RootResourceId,
            PathPart = "pets",
        });
        var pet = await apigw.CreateResourceAsync(new CreateResourceRequest
        {
            RestApiId = api.Id,
            ParentId = pets.Id,
            PathPart = "{id}",
        });
        await apigw.PutMethodAsync(new PutMethodRequest
        {
            RestApiId = api.Id,
            ResourceId = pet.Id,
            HttpMethod = "GET",
            AuthorizationType = "NONE",
        });
        await apigw.PutIntegrationAsync(new PutIntegrationRequest
        {
            RestApiId = api.Id,
            ResourceId = pet.Id,
            HttpMethod = "GET",
            Type = IntegrationType.AWS_PROXY,
            IntegrationHttpMethod = "POST",
            Uri = $"arn:aws:apigateway:us-east-1:lambda:path/2015-03-31/functions/{fn.FunctionArn}/invocations",
        });
        await apigw.CreateDeploymentAsync(new CreateDeploymentRequest
        {
            RestApiId = api.Id,
            StageName = "prod",
            Variables = new Dictionary<string, string> { ["env"] = "test" },
        });

        using var http = new HttpClient(mock.Handler, disposeHandler: false);
        var url = $"https://{api.Id}.execute-api.us-east-1.amazonaws.com/prod";
        var res = await http.GetAsync(new Uri($"{url}/pets/7"));
        Assert.Equal(HttpStatusCode.OK, res.StatusCode);
        Assert.Equal("7", res.Headers.GetValues("x-pet").Single());
        Assert.Equal("{\"id\":\"7\",\"env\":\"test\"}", await res.Content.ReadAsStringAsync());

        var missing = await http.GetAsync(new Uri($"{url}/nothing"));
        Assert.Equal(HttpStatusCode.Forbidden, missing.StatusCode);
        Assert.Equal("{\"message\":\"Missing Authentication Token\"}", await missing.Content.ReadAsStringAsync());
    }
}

using System.Text;
using Amazon.Lambda;
using Amazon.Lambda.Model;
using Amazon.SQS;

namespace Wasmo.Tests;

public class LambdaTest
{
    private static async Task CreateFunctionAsync(AmazonLambdaClient lambda, string name)
    {
        await lambda.CreateFunctionAsync(new CreateFunctionRequest
        {
            FunctionName = name,
            Runtime = Runtime.Dotnet8,
            Role = "arn:aws:iam::000000000000:role/lambda",
            Handler = "App::App.Function::Handler",
            Code = new FunctionCode { ZipFile = new MemoryStream(Encoding.UTF8.GetBytes("PK fake zip")) },
            Environment = new Amazon.Lambda.Model.Environment { Variables = new() { ["GREETING"] = "hello" } },
        });
    }

    [Fact]
    public async Task InvokeRunsTheRegisteredHandler()
    {
        using var mock = WasmoMock.Create();
        using var lambda = new AmazonLambdaClient(mock.Configure(new AmazonLambdaConfig()));
        using var sqs = new AmazonSQSClient(mock.Configure(new AmazonSQSConfig()));
        var queueUrl = (await sqs.CreateQueueAsync("seen")).QueueUrl;
        await CreateFunctionAsync(lambda, "greet");
        mock.RegisterFunction("greet", async (eventJson, context) =>
        {
            context.Log($"event {eventJson}");
            await sqs.SendMessageAsync(queueUrl, eventJson);
            return $"\"{context.Environment["GREETING"]} from {context.FunctionName}\"";
        });

        var response = await lambda.InvokeAsync(new InvokeRequest
        {
            FunctionName = "greet",
            Payload = "{\"n\":1}",
            LogType = LogType.Tail,
        });
        Assert.Equal(200, response.StatusCode);
        Assert.Null(response.FunctionError);
        Assert.Equal("\"hello from greet\"", Encoding.UTF8.GetString(response.Payload.ToArray()));
        var log = Encoding.UTF8.GetString(Convert.FromBase64String(response.LogResult));
        Assert.Contains("event {\"n\":1}\nEND RequestId", log, StringComparison.Ordinal);
        var received = await sqs.ReceiveMessageAsync(queueUrl);
        Assert.Equal("{\"n\":1}", Assert.Single(received.Messages).Body);
    }

    [Fact]
    public async Task ExceptionsAreFunctionErrors()
    {
        using var mock = WasmoMock.Create();
        using var lambda = new AmazonLambdaClient(mock.Configure(new AmazonLambdaConfig()));
        await CreateFunctionAsync(lambda, "boom");
        mock.RegisterFunction("boom", string? (_, _) => throw new InvalidOperationException("bad input"));
        var response = await lambda.InvokeAsync(new InvokeRequest { FunctionName = "boom" });
        Assert.Equal("Unhandled", response.FunctionError);
        var body = Encoding.UTF8.GetString(response.Payload.ToArray());
        Assert.StartsWith("{\"errorMessage\":\"bad input\",\"errorType\":\"InvalidOperationException\"", body, StringComparison.Ordinal);
    }
}

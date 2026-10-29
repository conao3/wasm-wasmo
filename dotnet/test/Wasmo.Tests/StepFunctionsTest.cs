using System.Text;
using System.Text.Json;
using Amazon.Lambda;
using Amazon.Lambda.Model;
using Amazon.StepFunctions;
using Amazon.StepFunctions.Model;

namespace Wasmo.Tests;

public class StepFunctionsTest
{
    private const string Role = "arn:aws:iam::000000000000:role/sfn";

    private static async Task CreateFunctionAsync(AmazonLambdaClient lambda, string name)
    {
        await lambda.CreateFunctionAsync(new CreateFunctionRequest
        {
            FunctionName = name,
            Runtime = Runtime.Dotnet8,
            Role = "arn:aws:iam::000000000000:role/lambda",
            Handler = "App::App.Function::Handler",
            Code = new FunctionCode { ZipFile = new MemoryStream(Encoding.UTF8.GetBytes("PK fake zip")) },
        });
    }

    [Fact]
    public async Task StateMachineRetriesAndCatchesHandlerErrors()
    {
        using var mock = WasmoMock.Create();
        using var lambda = new AmazonLambdaClient(mock.Configure(new AmazonLambdaConfig()));
        using var sfn = new AmazonStepFunctionsClient(mock.Configure(new AmazonStepFunctionsConfig()));
        await CreateFunctionAsync(lambda, "flaky");
        await CreateFunctionAsync(lambda, "broken");
        var calls = 0;
        mock.RegisterFunction("flaky", (eventJson, _) =>
        {
            if (++calls < 2)
            {
                throw new TimeoutException("not yet");
            }
            var n = JsonDocument.Parse(eventJson).RootElement.GetProperty("n").GetInt32();
            return $"{{\"doubled\":{n * 2}}}";
        });
        mock.RegisterFunction("broken", async (_, _) =>
        {
            await Task.Yield();
            throw new ArgumentException("always");
        });
        var definition = """
            {"StartAt": "Flaky", "States": {
              "Flaky": {"Type": "Task", "Resource": "arn:aws:states:::lambda:invoke",
                "Parameters": {"FunctionName": "flaky", "Payload.$": "$"},
                "ResultSelector": {"doubled.$": "$.Payload.doubled"},
                "ResultPath": "$.result",
                "Retry": [{"ErrorEquals": ["TimeoutException"], "MaxAttempts": 2}],
                "Next": "Broken"},
              "Broken": {"Type": "Task", "Resource": "arn:aws:states:::lambda:invoke",
                "Parameters": {"FunctionName": "broken", "Payload": {}},
                "Catch": [{"ErrorEquals": ["ArgumentException"], "ResultPath": "$.caught", "Next": "Done"}],
                "Next": "Done"},
              "Done": {"Type": "Succeed"}}}
            """;
        var machine = await sfn.CreateStateMachineAsync(new CreateStateMachineRequest
        {
            Name = "retry-catch",
            RoleArn = Role,
            Definition = definition,
        });
        var started = await sfn.StartExecutionAsync(new StartExecutionRequest
        {
            StateMachineArn = machine.StateMachineArn,
            Input = "{\"n\":21}",
        });
        var described = await sfn.DescribeExecutionAsync(new DescribeExecutionRequest { ExecutionArn = started.ExecutionArn });
        Assert.Equal(Amazon.StepFunctions.ExecutionStatus.SUCCEEDED, described.Status);
        var output = JsonDocument.Parse(described.Output).RootElement;
        Assert.Equal(42, output.GetProperty("result").GetProperty("doubled").GetInt32());
        Assert.Equal("ArgumentException", output.GetProperty("caught").GetProperty("Error").GetString());
        Assert.Equal(2, calls);
    }

    [Fact]
    public async Task SyncExecutionWaitsForTheAsyncHandler()
    {
        using var mock = WasmoMock.Create();
        using var lambda = new AmazonLambdaClient(mock.Configure(new AmazonLambdaConfig()));
        using var sfn = new AmazonStepFunctionsClient(mock.Configure(new AmazonStepFunctionsConfig()));
        await CreateFunctionAsync(lambda, "greet");
        mock.RegisterFunction("greet", async (eventJson, _) =>
        {
            await Task.Delay(1);
            var name = JsonDocument.Parse(eventJson).RootElement.GetProperty("name").GetString();
            return $"\"hello {name}\"";
        });
        var definition = """
            {"StartAt": "Greet", "States": {
              "Greet": {"Type": "Task", "Resource": "arn:aws:states:::lambda:invoke",
                "Parameters": {"FunctionName": "greet", "Payload.$": "$"},
                "OutputPath": "$.Payload", "End": true}}}
            """;
        var machine = await sfn.CreateStateMachineAsync(new CreateStateMachineRequest
        {
            Name = "sync",
            RoleArn = Role,
            Type = StateMachineType.EXPRESS,
            Definition = definition,
        });
        var output = await sfn.StartSyncExecutionAsync(new StartSyncExecutionRequest
        {
            StateMachineArn = machine.StateMachineArn,
            Input = "{\"name\":\"ann\"}",
        });
        Assert.Equal(SyncExecutionStatus.SUCCEEDED, output.Status);
        Assert.Equal("\"hello ann\"", output.Output);
    }
}

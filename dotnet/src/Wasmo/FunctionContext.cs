using System.Text;
using System.Text.Json;

namespace Wasmo;

/// <summary>The invocation a registered Lambda handler runs, as the Lambda context object describes it.</summary>
public sealed class FunctionContext
{
    private readonly StringBuilder _log = new();
    private readonly long _deadlineMs;

    internal FunctionContext(JsonElement meta)
    {
        FunctionName = meta.GetProperty("functionName").GetString()!;
        FunctionVersion = meta.GetProperty("functionVersion").GetString()!;
        InvokedFunctionArn = meta.GetProperty("invokedFunctionArn").GetString()!;
        AwsRequestId = meta.GetProperty("awsRequestId").GetString()!;
        LogGroupName = meta.GetProperty("logGroupName").GetString()!;
        LogStreamName = meta.GetProperty("logStreamName").GetString()!;
        InvocationType = meta.GetProperty("invocationType").GetString()!;
        MemoryLimitInMB = meta.GetProperty("memoryLimitInMB").GetInt32();
        ClientContext = meta.TryGetProperty("clientContext", out var c) ? c.GetString() : null;
        Environment = meta.GetProperty("environment").EnumerateObject().ToDictionary(p => p.Name, p => p.Value.GetString()!);
        _deadlineMs = (long)meta.GetProperty("deadlineMs").GetDouble();
    }

    /// <summary>The function name.</summary>
    public string FunctionName { get; }

    /// <summary>The executed version (<c>$LATEST</c> or a number).</summary>
    public string FunctionVersion { get; }

    /// <summary>The ARN the caller invoked, with its qualifier.</summary>
    public string InvokedFunctionArn { get; }

    /// <summary>The request id of the invocation.</summary>
    public string AwsRequestId { get; }

    /// <summary>The CloudWatch Logs group of the function.</summary>
    public string LogGroupName { get; }

    /// <summary>The CloudWatch Logs stream of the invocation.</summary>
    public string LogStreamName { get; }

    /// <summary><c>RequestResponse</c> or <c>Event</c>.</summary>
    public string InvocationType { get; }

    /// <summary>The configured memory size.</summary>
    public int MemoryLimitInMB { get; }

    /// <summary>The decoded client context of a synchronous invocation.</summary>
    public string? ClientContext { get; }

    /// <summary>The function's environment variables and the ones Lambda sets (<c>AWS_REGION</c>, ...).</summary>
    public IReadOnlyDictionary<string, string> Environment { get; }

    /// <summary>The time left before the function's timeout.</summary>
    public TimeSpan RemainingTime =>
        TimeSpan.FromMilliseconds(Math.Max(0, _deadlineMs - DateTimeOffset.UtcNow.ToUnixTimeMilliseconds()));

    /// <summary>Appends a line to the invocation log that Invoke returns with LogType Tail.</summary>
    public void Log(string message)
    {
        lock (_log)
        {
            _log.Append(message);
            if (!message.EndsWith('\n'))
            {
                _log.Append('\n');
            }
        }
    }

    internal string LogText()
    {
        lock (_log)
        {
            return _log.ToString();
        }
    }
}

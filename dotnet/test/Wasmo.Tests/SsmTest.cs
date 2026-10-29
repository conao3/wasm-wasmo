using Amazon.SimpleSystemsManagement;
using Amazon.SimpleSystemsManagement.Model;

namespace Wasmo.Tests;

public class SsmTest
{
    [Fact]
    public async Task GetsParametersByPath()
    {
        using var mock = WasmoMock.Create();
        using var ssm = new AmazonSimpleSystemsManagementClient(mock.Configure(new AmazonSimpleSystemsManagementConfig()));
        foreach (var name in new[] { "/app/db/host", "/app/db/port", "/app/api/url", "/other/x" })
        {
            await ssm.PutParameterAsync(new PutParameterRequest { Name = name, Value = name[(name.LastIndexOf('/') + 1)..], Type = ParameterType.String });
        }
        await ssm.PutParameterAsync(new PutParameterRequest { Name = "/app/db/password", Value = "hunter2", Type = ParameterType.SecureString });

        var shallow = await ssm.GetParametersByPathAsync(new GetParametersByPathRequest { Path = "/app" });
        Assert.True(shallow.Parameters is null || shallow.Parameters.Count == 0);

        var found = new Dictionary<string, string>();
        var paginator = ssm.Paginators.GetParametersByPath(new GetParametersByPathRequest { Path = "/app", Recursive = true, WithDecryption = true, MaxResults = 2 });
        await foreach (var page in paginator.Responses)
        {
            foreach (var p in page.Parameters ?? [])
            {
                found[p.Name] = p.Value;
            }
        }
        Assert.Equal(new Dictionary<string, string>
        {
            ["/app/db/host"] = "host",
            ["/app/db/port"] = "port",
            ["/app/db/password"] = "hunter2",
            ["/app/api/url"] = "url",
        }, found);

        var db = (await ssm.GetParametersByPathAsync(new GetParametersByPathRequest { Path = "/app/db/" })).Parameters;
        Assert.Equal(["/app/db/host", "/app/db/password", "/app/db/port"], db.Select(p => p.Name).Order());
        var secret = db.Single(p => p.Name == "/app/db/password");
        Assert.Equal(ParameterType.SecureString, secret.Type);
        Assert.NotEqual("hunter2", secret.Value);
    }
}

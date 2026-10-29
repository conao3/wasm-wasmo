using Amazon.SecretsManager;
using Amazon.SecretsManager.Model;

namespace Wasmo.Tests;

public class SecretsManagerTest
{
    [Fact]
    public async Task KeepsVersionsAndStages()
    {
        using var mock = WasmoMock.Create();
        using var sm = new AmazonSecretsManagerClient(mock.Configure(new AmazonSecretsManagerConfig()));
        var first = await sm.CreateSecretAsync(new CreateSecretRequest { Name = "db", SecretString = "v1" });
        var second = await sm.PutSecretValueAsync(new PutSecretValueRequest { SecretId = "db", SecretString = "v2" });

        Assert.Equal("v2", (await sm.GetSecretValueAsync(new GetSecretValueRequest { SecretId = "db" })).SecretString);
        var previous = await sm.GetSecretValueAsync(new GetSecretValueRequest { SecretId = "db", VersionStage = "AWSPREVIOUS" });
        Assert.Equal("v1", previous.SecretString);
        Assert.Equal(first.VersionId, previous.VersionId);

        var versions = (await sm.ListSecretVersionIdsAsync(new ListSecretVersionIdsRequest { SecretId = "db" })).Versions;
        var stages = versions.ToDictionary(v => v.VersionId, v => string.Join(",", v.VersionStages));
        Assert.Equal(new Dictionary<string, string> { [first.VersionId] = "AWSPREVIOUS", [second.VersionId] = "AWSCURRENT" }, stages);

        byte[] binary = [0, 1, 255];
        await sm.PutSecretValueAsync(new PutSecretValueRequest { SecretId = "db", SecretBinary = new MemoryStream(binary) });
        Assert.Equal(binary, (await sm.GetSecretValueAsync(new GetSecretValueRequest { SecretId = "db" })).SecretBinary.ToArray());
        await Assert.ThrowsAsync<ResourceNotFoundException>(() => sm.GetSecretValueAsync(new GetSecretValueRequest { SecretId = "missing" }));
    }
}

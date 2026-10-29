using Amazon.SecurityToken;
using Amazon.SecurityToken.Model;

namespace Wasmo.Tests;

public class StsTest
{
    [Fact]
    public async Task ReturnsTheCallerIdentity()
    {
        using var mock = WasmoMock.Create();
        using var sts = new AmazonSecurityTokenServiceClient(mock.Configure(new AmazonSecurityTokenServiceConfig()));
        var identity = await sts.GetCallerIdentityAsync(new GetCallerIdentityRequest());
        Assert.Equal("000000000000", identity.Account);
    }
}

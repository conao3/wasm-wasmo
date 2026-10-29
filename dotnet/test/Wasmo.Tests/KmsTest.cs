using System.Text;
using Amazon.KeyManagementService;
using Amazon.KeyManagementService.Model;

namespace Wasmo.Tests;

public class KmsTest
{
    [Fact]
    public async Task EncryptsAndDecrypts()
    {
        using var mock = WasmoMock.Create();
        using var kms = new AmazonKeyManagementServiceClient(mock.Configure(new AmazonKeyManagementServiceConfig()));
        var keyId = (await kms.CreateKeyAsync(new CreateKeyRequest())).KeyMetadata.KeyId;
        var plaintext = Encoding.UTF8.GetBytes("top secret");
        var context = new Dictionary<string, string> { ["purpose"] = "test" };

        var encrypted = await kms.EncryptAsync(new EncryptRequest { KeyId = keyId, Plaintext = new MemoryStream(plaintext), EncryptionContext = context });
        Assert.NotEqual(plaintext, encrypted.CiphertextBlob.ToArray());

        var decrypted = await kms.DecryptAsync(new DecryptRequest { CiphertextBlob = encrypted.CiphertextBlob, EncryptionContext = context });
        Assert.Equal(plaintext, decrypted.Plaintext.ToArray());
        Assert.EndsWith(keyId, decrypted.KeyId);

        await Assert.ThrowsAsync<InvalidCiphertextException>(() =>
            kms.DecryptAsync(new DecryptRequest { CiphertextBlob = new MemoryStream(encrypted.CiphertextBlob.ToArray()) }));
    }
}

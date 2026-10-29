using Amazon.Runtime;

namespace Wasmo.Tests;

internal sealed class Recorder(HttpMessageHandler inner) : DelegatingHandler(inner)
{
    public List<HttpRequestMessage> Requests { get; } = [];

    public static Recorder Attach(WasmoMock mock, ClientConfig config)
    {
        var recorder = new Recorder(mock.Handler);
        config.HttpClientFactory = new Factory(recorder);
        return recorder;
    }

    protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken)
    {
        lock (Requests)
        {
            Requests.Add(request);
        }
        return base.SendAsync(request, cancellationToken);
    }

    private sealed class Factory(Recorder recorder) : HttpClientFactory
    {
        public override HttpClient CreateHttpClient(IClientConfig clientConfig) => new(recorder, disposeHandler: false);

        public override bool UseSDKHttpClientCaching(IClientConfig clientConfig) => false;

        public override bool DisposeHttpClientsAfterUse(IClientConfig clientConfig) => false;
    }
}

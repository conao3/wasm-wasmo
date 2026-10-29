using System.Buffers.Binary;
using System.Security.Cryptography;
using Amazon;
using Amazon.Runtime;
using Wasmtime;

namespace Wasmo;

/// <summary>
/// An isolated in-process AWS. Each mock owns its own WebAssembly instance, so state never leaks
/// between mocks. Pass a client config through <see cref="Configure{T}(T)"/> to point an AWS SDK
/// client at it.
/// </summary>
public sealed class WasmoMock : IDisposable
{
    /// <summary>The service URL every configured client sends to.</summary>
    public static readonly Uri Endpoint = new("http://wasmo.local");

    /// <summary>The region configured clients sign for.</summary>
    public static readonly RegionEndpoint Region = RegionEndpoint.USEast1;

    /// <summary>Dummy credentials accepted by the mock.</summary>
    public static readonly AWSCredentials Credentials = new BasicAWSCredentials("test", "test");

    private static readonly Lazy<Runtime> SharedRuntime = new(Runtime.Load);

    private readonly object _gate = new();
    private readonly Store _store;
    private readonly Memory _memory;
    private readonly Func<int, int> _alloc;
    private readonly Action<int, int> _free;
    private readonly Func<int, int, int, int, long> _handle;
    private readonly Handler _handler;
    private bool _disposed;

    private WasmoMock()
    {
        var runtime = SharedRuntime.Value;
        _store = new Store(runtime.Engine);
        var instance = runtime.Linker.Instantiate(_store, runtime.Module);
        _memory = instance.GetMemory("memory") ?? throw Missing("memory");
        _alloc = instance.GetFunction<int, int>("wasmo_alloc") ?? throw Missing("wasmo_alloc");
        _free = instance.GetAction<int, int>("wasmo_free") ?? throw Missing("wasmo_free");
        _handle = instance.GetFunction<int, int, int, int, long>("wasmo_handle") ?? throw Missing("wasmo_handle");
        _handler = new Handler(this);
    }

    /// <summary>Starts a mock with empty state.</summary>
    public static WasmoMock Create() => new();

    /// <summary>
    /// Points <paramref name="config"/> at this mock: service URL, signing region, dummy
    /// credentials, and an HTTP client factory that routes every request into the instance.
    /// </summary>
    public T Configure<T>(T config) where T : ClientConfig
    {
        config.ServiceURL = Endpoint.ToString();
        config.AuthenticationRegion = Region.SystemName;
        config.DefaultAWSCredentials = Credentials;
        config.HttpClientFactory = new ClientFactory(_handler);
        return config;
    }

    /// <summary>An <see cref="HttpMessageHandler"/> that answers every request from this mock.</summary>
    public HttpMessageHandler Handler => _handler;

    /// <summary>Releases the WebAssembly instance. Later requests throw.</summary>
    public void Dispose()
    {
        lock (_gate)
        {
            if (_disposed)
            {
                return;
            }
            _disposed = true;
            _store.Dispose();
        }
    }

    internal byte[] Roundtrip(ReadOnlySpan<byte> meta, ReadOnlySpan<byte> body)
    {
        lock (_gate)
        {
            ObjectDisposedException.ThrowIf(_disposed, this);
            var metaPtr = Put(meta);
            var bodyPtr = Put(body);
            var res = (ulong)_handle(metaPtr, meta.Length, bodyPtr, body.Length);
            var ptr = (int)(res >> 32);
            var len = (int)(uint)res;
            var frame = _memory.GetSpan((uint)ptr, len).ToArray();
            _free(ptr, len);
            return frame;
        }
    }

    private int Put(ReadOnlySpan<byte> data)
    {
        var ptr = _alloc(data.Length);
        if (data.Length > 0)
        {
            data.CopyTo(_memory.GetSpan((uint)ptr, data.Length));
        }
        return ptr;
    }

    internal static int MetaLength(byte[] frame) => (int)BinaryPrimitives.ReadUInt32BigEndian(frame);

    private static InvalidOperationException Missing(string name) => new($"wasmo: module does not export {name}");

    private sealed class Runtime
    {
        public required Engine Engine { get; init; }
        public required Module Module { get; init; }
        public required Linker Linker { get; init; }

        public static Runtime Load()
        {
            using var stream = typeof(WasmoMock).Assembly.GetManifestResourceStream("wasmo.wasm")
                ?? throw new InvalidOperationException("wasmo: embedded wasmo.wasm is missing");
            using var bytes = new MemoryStream();
            stream.CopyTo(bytes);
            var engine = new Engine();
            var module = Module.FromBytes(engine, "wasmo", bytes.ToArray());
            var linker = new Linker(engine);
            linker.DefineFunction("wasmo", "now_ms", () => DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() * 1.0);
            linker.DefineFunction("wasmo", "random_fill", (Caller caller, int ptr, int len) =>
            {
                var memory = caller.GetMemory("memory") ?? throw Missing("memory");
                RandomNumberGenerator.Fill(memory.GetSpan((uint)ptr, len));
            });
            return new Runtime { Engine = engine, Module = module, Linker = linker };
        }
    }

    private sealed class ClientFactory(Handler handler) : HttpClientFactory
    {
        public override HttpClient CreateHttpClient(IClientConfig clientConfig) =>
            new(handler, disposeHandler: false) { Timeout = Timeout.InfiniteTimeSpan };

        public override bool UseSDKHttpClientCaching(IClientConfig clientConfig) => false;

        public override bool DisposeHttpClientsAfterUse(IClientConfig clientConfig) => false;

        public override string GetConfigUniqueString(IClientConfig clientConfig) => "wasmo";
    }
}

using System.Net;
using System.Net.Http.Headers;
using System.Text.Json;

namespace Wasmo;

internal sealed class Handler(WasmoMock mock) : HttpMessageHandler
{
    protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken)
    {
        var body = request.Content is null ? [] : await request.Content.ReadAsByteArrayAsync(cancellationToken).ConfigureAwait(false);
        return Respond(request, body);
    }

    protected override HttpResponseMessage Send(HttpRequestMessage request, CancellationToken cancellationToken)
    {
        byte[] body = [];
        if (request.Content is not null)
        {
            using var stream = request.Content.ReadAsStream(cancellationToken);
            using var buffer = new MemoryStream();
            stream.CopyTo(buffer);
            body = buffer.ToArray();
        }
        return Respond(request, body);
    }

    private HttpResponseMessage Respond(HttpRequestMessage request, byte[] body)
    {
        var frame = mock.Roundtrip(EncodeMeta(request), body);
        var metaLen = WasmoMock.MetaLength(frame);
        using var meta = JsonDocument.Parse(frame.AsMemory(4, metaLen));
        var root = meta.RootElement;
        var offset = 4 + metaLen;
        var content = new StreamContent(new MemoryStream(frame, offset, frame.Length - offset, writable: false));
        var response = new HttpResponseMessage((HttpStatusCode)root.GetProperty("status").GetInt32())
        {
            Content = content,
            RequestMessage = request,
        };
        foreach (var pair in root.GetProperty("headers").EnumerateArray())
        {
            var name = pair[0].GetString()!;
            var value = pair[1].GetString()!;
            if (!response.Headers.TryAddWithoutValidation(name, value))
            {
                content.Headers.TryAddWithoutValidation(name, value);
            }
        }
        return response;
    }

    private static byte[] EncodeMeta(HttpRequestMessage request)
    {
        var uri = request.RequestUri ?? throw new InvalidOperationException("wasmo: request has no URI");
        using var buffer = new MemoryStream();
        using (var json = new Utf8JsonWriter(buffer))
        {
            json.WriteStartObject();
            json.WriteString("method", request.Method.Method);
            json.WriteString("url", uri.AbsoluteUri);
            json.WriteStartArray("headers");
            if (request.Headers.Host is null)
            {
                WriteHeader(json, "host", uri.Authority);
            }
            WriteHeaders(json, request.Headers);
            if (request.Content is not null)
            {
                WriteHeaders(json, request.Content.Headers);
            }
            json.WriteEndArray();
            json.WriteEndObject();
        }
        return buffer.ToArray();
    }

    private static void WriteHeaders(Utf8JsonWriter json, HttpHeaders headers)
    {
        foreach (var (name, values) in headers.NonValidated)
        {
            WriteHeader(json, name.ToLowerInvariant(), string.Join(",", values));
        }
    }

    private static void WriteHeader(Utf8JsonWriter json, string name, string value)
    {
        json.WriteStartArray();
        json.WriteStringValue(name);
        json.WriteStringValue(value);
        json.WriteEndArray();
    }
}

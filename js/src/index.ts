import { readFile } from "node:fs/promises";
import { Readable } from "node:stream";
import { format } from "node:util";
import type {
  AwsCredentialIdentity,
  HeaderBag,
  HttpHandlerOptions,
  HttpRequest,
  HttpResponse,
  QueryParameterBag,
  RequestHandler,
  RequestHandlerOutput,
} from "@smithy/types";

export const endpoint = "http://wasmo.local";
export const region = "us-east-1";

export interface WasmoRequestHandler extends RequestHandler<
  HttpRequest,
  HttpResponse,
  HttpHandlerOptions
> {
  updateHttpClientConfig(key: never, value: never): void;
  httpHandlerConfigs(): Record<string, never>;
}

export interface WasmoClientConfig {
  endpoint: string;
  region: string;
  credentials: AwsCredentialIdentity;
  requestHandler: WasmoRequestHandler;
}

export interface FunctionContext {
  readonly functionName: string;
  readonly functionVersion: string;
  readonly invokedFunctionArn: string;
  readonly memoryLimitInMB: string;
  readonly awsRequestId: string;
  readonly logGroupName: string;
  readonly logStreamName: string;
  readonly invocationType: string;
  readonly clientContext?: unknown;
  /** The function's environment variables and the ones Lambda sets (AWS_REGION, ...). */
  readonly environment: Readonly<Record<string, string>>;
  getRemainingTimeInMillis(): number;
  /** Appends a line to the invocation log that Invoke returns with LogType Tail. */
  log(...args: unknown[]): void;
}

export type FunctionHandler = (event: any, context: FunctionContext) => unknown;

export interface WasmoMock {
  readonly clientConfig: WasmoClientConfig;
  /** Binds a handler to every invocation of the Lambda function with this name. */
  registerFunction(name: string, handler: FunctionHandler): void;
  reset(): void;
  close(): void;
}

interface Exports {
  memory: WebAssembly.Memory;
  wasmo_alloc(len: number): number;
  wasmo_free(ptr: number, len: number): void;
  wasmo_handle(metaPtr: number, metaLen: number, bodyPtr: number, bodyLen: number): bigint;
  wasmo_complete(metaPtr: number, metaLen: number, payloadPtr: number, payloadLen: number): bigint;
}

interface Invocation {
  meta: Uint8Array;
  payload: Uint8Array;
}

interface InvocationMeta {
  id: string;
  functionName: string;
  functionVersion: string;
  invokedFunctionArn: string;
  memoryLimitInMB: number;
  deadlineMs: number;
  awsRequestId: string;
  logGroupName: string;
  logStreamName: string;
  invocationType: string;
  clientContext?: string;
  environment: Record<string, string>;
}

interface Queue {
  current: Invocation[] | undefined;
}

let compiled: Promise<WebAssembly.Module> | undefined;

function compile(): Promise<WebAssembly.Module> {
  compiled ??= readFile(new URL("../wasmo.wasm", import.meta.url)).then((bytes) =>
    WebAssembly.compile(bytes),
  );
  return compiled;
}

function instantiate(module: WebAssembly.Module, queue: Queue): Exports {
  let memory: WebAssembly.Memory | undefined;
  const copy = (ptr: number, len: number) =>
    new Uint8Array(memory!.buffer, ptr >>> 0, len >>> 0).slice();
  const instance = new WebAssembly.Instance(module, {
    wasmo: {
      now_ms: () => Date.now(),
      random_fill: (ptr: number, len: number) => {
        const view = new Uint8Array(memory!.buffer, ptr >>> 0, len >>> 0);
        for (let off = 0; off < view.length; off += 65536) {
          crypto.getRandomValues(view.subarray(off, off + 65536));
        }
      },
      invoke: (metaPtr: number, metaLen: number, payloadPtr: number, payloadLen: number) => {
        if (!queue.current) throw new Error("wasmo: invoke outside a request");
        queue.current.push({ meta: copy(metaPtr, metaLen), payload: copy(payloadPtr, payloadLen) });
      },
    },
  });
  const exports = instance.exports as unknown as Exports;
  memory = exports.memory;
  return exports;
}

const encoder = new TextEncoder();
const decoder = new TextDecoder();

function escapeUri(s: string): string {
  return encodeURIComponent(s).replace(
    /[!'()*]/g,
    (c) => `%${c.charCodeAt(0).toString(16).toUpperCase()}`,
  );
}

function queryString(query: QueryParameterBag): string {
  const parts: string[] = [];
  for (const key of Object.keys(query).sort()) {
    const value = query[key];
    const k = escapeUri(key);
    if (Array.isArray(value)) {
      for (const v of value) parts.push(`${k}=${escapeUri(v)}`);
    } else if (typeof value === "string") {
      parts.push(`${k}=${escapeUri(value)}`);
    } else {
      parts.push(k);
    }
  }
  return parts.join("&");
}

function requestUrl(req: HttpRequest): string {
  const port = req.port ? `:${req.port}` : "";
  const qs = req.query ? queryString(req.query) : "";
  return `${req.protocol}//${req.hostname}${port}${req.path}${qs ? `?${qs}` : ""}`;
}

function concat(chunks: Uint8Array[]): Uint8Array {
  if (chunks.length === 1) return chunks[0]!;
  let len = 0;
  for (const c of chunks) len += c.length;
  const out = new Uint8Array(len);
  let off = 0;
  for (const c of chunks) {
    out.set(c, off);
    off += c.length;
  }
  return out;
}

function toBytes(chunk: unknown): Uint8Array {
  if (typeof chunk === "string") return encoder.encode(chunk);
  if (chunk instanceof Uint8Array) return chunk;
  if (chunk instanceof ArrayBuffer) return new Uint8Array(chunk);
  if (ArrayBuffer.isView(chunk)) {
    return new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength);
  }
  throw new TypeError(`wasmo: unsupported body chunk ${Object.prototype.toString.call(chunk)}`);
}

async function readBody(body: unknown): Promise<Uint8Array> {
  if (body == null) return new Uint8Array(0);
  if (
    typeof body === "string" ||
    body instanceof Uint8Array ||
    body instanceof ArrayBuffer ||
    ArrayBuffer.isView(body)
  ) {
    return toBytes(body);
  }
  if (typeof Blob !== "undefined" && body instanceof Blob) {
    return new Uint8Array(await body.arrayBuffer());
  }
  if (typeof (body as AsyncIterable<unknown>)[Symbol.asyncIterator] === "function") {
    const chunks: Uint8Array[] = [];
    for await (const chunk of body as AsyncIterable<unknown>) chunks.push(toBytes(chunk));
    return concat(chunks);
  }
  if (typeof (body as ReadableStream).getReader === "function") {
    const reader = (body as ReadableStream<unknown>).getReader();
    const chunks: Uint8Array[] = [];
    for (;;) {
      const { done, value } = await reader.read();
      if (done) return concat(chunks);
      chunks.push(toBytes(value));
    }
  }
  throw new TypeError(`wasmo: unsupported request body ${Object.prototype.toString.call(body)}`);
}

function put(ex: Exports, data: Uint8Array): number {
  const ptr = ex.wasmo_alloc(data.length) >>> 0;
  if (data.length > 0) new Uint8Array(ex.memory.buffer, ptr, data.length).set(data);
  return ptr;
}

function roundtrip(
  ex: Exports,
  fn: (a: number, b: number, c: number, d: number) => bigint,
  meta: Uint8Array,
  body: Uint8Array,
): Uint8Array | undefined {
  const metaPtr = put(ex, meta);
  const bodyPtr = put(ex, body);
  const res = fn(metaPtr, meta.length, bodyPtr, body.length);
  if (res === 0n) return undefined;
  const ptr = Number(res >> 32n);
  const len = Number(res & 0xffffffffn);
  const frame = new Uint8Array(ex.memory.buffer, ptr, len).slice();
  ex.wasmo_free(ptr, len);
  return frame;
}

function responseBody(bytes: Uint8Array): Readable {
  return new Readable({
    read() {
      if (bytes.length > 0) this.push(bytes);
      this.push(null);
    },
  });
}

function errorOf(e: unknown): { type: string; message: string; stackTrace?: string[] } {
  if (e instanceof Error) {
    const stackTrace = e.stack
      ?.split("\n")
      .slice(1)
      .map((line) => line.trim());
    return { type: e.name, message: e.message, ...(stackTrace ? { stackTrace } : {}) };
  }
  return { type: typeof e, message: String(e) };
}

function parseClientContext(raw: string | undefined): unknown {
  if (raw === undefined) return undefined;
  try {
    return JSON.parse(raw);
  } catch {
    return raw;
  }
}

async function run(
  handlers: Map<string, FunctionHandler>,
  inv: Invocation,
): Promise<[Uint8Array, Uint8Array]> {
  const meta = JSON.parse(decoder.decode(inv.meta)) as InvocationMeta;
  const handler = handlers.get(meta.functionName);
  const empty = new Uint8Array(0);
  if (!handler) return [encoder.encode(JSON.stringify({ id: meta.id, registered: false })), empty];
  const lines: string[] = [];
  const context: FunctionContext = {
    functionName: meta.functionName,
    functionVersion: meta.functionVersion,
    invokedFunctionArn: meta.invokedFunctionArn,
    memoryLimitInMB: String(meta.memoryLimitInMB),
    awsRequestId: meta.awsRequestId,
    logGroupName: meta.logGroupName,
    logStreamName: meta.logStreamName,
    invocationType: meta.invocationType,
    clientContext: parseClientContext(meta.clientContext),
    environment: meta.environment,
    getRemainingTimeInMillis: () => Math.max(0, meta.deadlineMs - Date.now()),
    log: (...args) => lines.push(format(...args)),
  };
  const log = () => lines.map((l) => `${l}\n`).join("");
  try {
    const event: unknown = JSON.parse(decoder.decode(inv.payload));
    const out: unknown = await handler(event, context);
    return [
      encoder.encode(JSON.stringify({ id: meta.id, log: log() })),
      encoder.encode(JSON.stringify(out ?? null)),
    ];
  } catch (e) {
    return [encoder.encode(JSON.stringify({ id: meta.id, error: errorOf(e), log: log() })), empty];
  }
}

function abortError(signal: { reason?: unknown }): Error {
  if (signal.reason instanceof Error) return signal.reason;
  const err = new Error("Request aborted");
  err.name = "AbortError";
  return err;
}

export function mockAws(): WasmoMock {
  let instance: Exports | undefined;
  let closed = false;
  const queue: Queue = { current: undefined };
  const handlers = new Map<string, FunctionHandler>();
  compile().catch(() => {});

  async function exportsFor(): Promise<Exports> {
    if (closed) throw new Error("wasmo: mock is closed");
    if (instance) return instance;
    const module = await compile();
    if (closed) throw new Error("wasmo: mock is closed");
    instance ??= instantiate(module, queue);
    return instance;
  }

  const requestHandler: WasmoRequestHandler = {
    metadata: { handlerProtocol: "http/1.1" },
    async handle(request, options): Promise<RequestHandlerOutput<HttpResponse>> {
      const signal = options?.abortSignal;
      if (signal?.aborted) throw abortError(signal as { reason?: unknown });
      const body = await readBody(request.body);
      const headers: [string, string][] = [];
      let hasHost = false;
      for (const [k, v] of Object.entries(request.headers)) {
        const name = k.toLowerCase();
        if (name === "host") hasHost = true;
        headers.push([name, v]);
      }
      if (!hasHost) {
        headers.push([
          "host",
          request.port ? `${request.hostname}:${request.port}` : request.hostname,
        ]);
      }
      const meta = encoder.encode(
        JSON.stringify({ method: request.method, url: requestUrl(request), headers }),
      );
      const ex = await exportsFor();
      if (signal?.aborted) throw abortError(signal as { reason?: unknown });
      const pending: Invocation[] = [];
      const call = (fn: Exports["wasmo_handle"], m: Uint8Array, b: Uint8Array) => {
        queue.current = pending;
        try {
          return roundtrip(ex, fn, m, b);
        } finally {
          queue.current = undefined;
        }
      };
      let frame = call(ex.wasmo_handle, meta, body)!;
      while (pending.length > 0) {
        const [resMeta, payload] = await run(handlers, pending.shift()!);
        frame = call(ex.wasmo_complete, resMeta, payload) ?? frame;
      }
      const view = new DataView(frame.buffer, frame.byteOffset, frame.byteLength);
      const metaLen = view.getUint32(0);
      const res = JSON.parse(decoder.decode(frame.subarray(4, 4 + metaLen))) as {
        await?: string;
        status: number;
        headers: [string, string][];
      };
      if (res.await) throw new Error(`wasmo: invocation ${res.await} was never answered`);
      const resHeaders: HeaderBag = {};
      for (const [k, v] of res.headers) {
        const name = k.toLowerCase();
        resHeaders[name] = name in resHeaders ? `${resHeaders[name]}, ${v}` : v;
      }
      return {
        response: {
          statusCode: res.status,
          headers: resHeaders,
          body: responseBody(frame.subarray(4 + metaLen)),
        },
      };
    },
    updateHttpClientConfig() {},
    httpHandlerConfigs() {
      return {};
    },
  };

  return {
    clientConfig: {
      endpoint,
      region,
      credentials: { accessKeyId: "test", secretAccessKey: "test" },
      requestHandler,
    },
    registerFunction(name, handler) {
      handlers.set(name, handler);
    },
    reset() {
      if (closed) throw new Error("wasmo: mock is closed");
      instance = undefined;
    },
    close() {
      closed = true;
      instance = undefined;
    },
  };
}

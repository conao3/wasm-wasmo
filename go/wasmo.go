// Package wasmo runs a stateful AWS mock inside the test process. Each Mock
// owns an independent WebAssembly instance, so state never leaks between
// mocks.
package wasmo

import (
	"bytes"
	"context"
	"crypto/rand"
	_ "embed"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/credentials"
	"github.com/tetratelabs/wazero"
	"github.com/tetratelabs/wazero/api"
)

//go:embed wasmo.wasm
var wasmBinary []byte

const endpoint = "http://wasmo.local"

type engine struct {
	runtime  wazero.Runtime
	compiled wazero.CompiledModule
}

var (
	engineOnce sync.Once
	sharedEng  *engine
	engineErr  error
)

func loadEngine(ctx context.Context) (*engine, error) {
	engineOnce.Do(func() {
		rt := wazero.NewRuntime(ctx)
		_, err := rt.NewHostModuleBuilder("wasmo").
			NewFunctionBuilder().WithFunc(func() float64 {
			return float64(time.Now().UnixNano()) / 1e6
		}).Export("now_ms").
			NewFunctionBuilder().WithFunc(func(_ context.Context, m api.Module, ptr, length uint32) {
			buf, ok := m.Memory().Read(ptr, length)
			if !ok {
				panic("wasmo: random_fill out of range")
			}
			if _, err := rand.Read(buf); err != nil {
				panic(err)
			}
		}).Export("random_fill").
			NewFunctionBuilder().WithFunc(func(ctx context.Context, m api.Module, metaPtr, metaLen, payloadPtr, payloadLen uint32) {
			q, ok := ctx.Value(queueKey{}).(*[]invocation)
			if !ok {
				panic("wasmo: invoke outside a request")
			}
			meta, ok1 := m.Memory().Read(metaPtr, metaLen)
			payload, ok2 := m.Memory().Read(payloadPtr, payloadLen)
			if !ok1 || !ok2 {
				panic("wasmo: invoke out of range")
			}
			*q = append(*q, invocation{meta: append([]byte(nil), meta...), payload: append([]byte(nil), payload...)})
		}).Export("invoke").
			Instantiate(ctx)
		if err != nil {
			engineErr = err
			return
		}
		compiled, err := rt.CompileModule(ctx, wasmBinary)
		if err != nil {
			engineErr = err
			return
		}
		sharedEng = &engine{runtime: rt, compiled: compiled}
	})
	return sharedEng, engineErr
}

// Mock is an isolated in-process AWS. It implements aws.HTTPClient.
type Mock struct {
	mu       sync.Mutex
	module   api.Module
	alloc    api.Function
	free     api.Function
	handle   api.Function
	complete api.Function

	fnMu      sync.Mutex
	functions map[string]Handler
}

// New starts a Mock with empty state.
func New(ctx context.Context) (*Mock, error) {
	eng, err := loadEngine(ctx)
	if err != nil {
		return nil, err
	}
	mod, err := eng.runtime.InstantiateModule(ctx, eng.compiled, wazero.NewModuleConfig().WithName(""))
	if err != nil {
		return nil, err
	}
	return &Mock{
		module:   mod,
		alloc:    mod.ExportedFunction("wasmo_alloc"),
		free:     mod.ExportedFunction("wasmo_free"),
		handle:   mod.ExportedFunction("wasmo_handle"),
		complete: mod.ExportedFunction("wasmo_complete"),
	}, nil
}

// Close releases the WebAssembly instance.
func (m *Mock) Close(ctx context.Context) error {
	return m.module.Close(ctx)
}

// Config returns an aws.Config whose clients talk to this Mock.
func (m *Mock) Config() aws.Config {
	return aws.Config{
		Region:       "us-east-1",
		Credentials:  credentials.NewStaticCredentialsProvider("test", "test", ""),
		HTTPClient:   m,
		BaseEndpoint: aws.String(endpoint),
	}
}

type requestMeta struct {
	Method  string      `json:"method"`
	URL     string      `json:"url"`
	Headers [][2]string `json:"headers"`
}

type responseMeta struct {
	Await   string      `json:"await"`
	Status  int         `json:"status"`
	Headers [][2]string `json:"headers"`
}

func (m *Mock) put(ctx context.Context, data []byte) (uint32, error) {
	res, err := m.alloc.Call(ctx, uint64(len(data)))
	if err != nil {
		return 0, err
	}
	ptr := uint32(res[0])
	if len(data) > 0 && !m.module.Memory().Write(ptr, data) {
		return 0, fmt.Errorf("wasmo: write out of range")
	}
	return ptr, nil
}

// Do sends one HTTP request to the mock.
func (m *Mock) Do(r *http.Request) (*http.Response, error) {
	var body []byte
	if r.Body != nil {
		var err error
		if body, err = io.ReadAll(r.Body); err != nil {
			return nil, err
		}
	}
	headers := [][2]string{{"host", r.URL.Host}}
	for k, vs := range r.Header {
		headers = append(headers, [2]string{strings.ToLower(k), strings.Join(vs, ",")})
	}
	meta, err := json.Marshal(requestMeta{Method: r.Method, URL: r.URL.String(), Headers: headers})
	if err != nil {
		return nil, err
	}

	res, respBody, err := m.exchange(r.Context(), meta, body)
	if err != nil {
		return nil, err
	}
	h := http.Header{}
	for _, kv := range res.Headers {
		h.Add(kv[0], kv[1])
	}
	return &http.Response{
		StatusCode:    res.Status,
		Status:        fmt.Sprintf("%d %s", res.Status, http.StatusText(res.Status)),
		Header:        h,
		Body:          io.NopCloser(bytes.NewReader(respBody)),
		ContentLength: int64(len(respBody)),
		Request:       r,
	}, nil
}

// exchange calls handle and then runs every Lambda invocation the module
// announces, outside the lock, answering each with wasmo_complete until the
// response no longer waits for one.
func (m *Mock) exchange(ctx context.Context, meta, body []byte) (*responseMeta, []byte, error) {
	var pending []invocation
	m.mu.Lock()
	frame, err := m.roundtrip(ctx, m.handle, &pending, meta, body)
	m.mu.Unlock()
	if err != nil {
		return nil, nil, err
	}
	for len(pending) > 0 {
		inv := pending[0]
		pending = pending[1:]
		resMeta, payload, err := m.run(ctx, inv)
		if err != nil {
			return nil, nil, err
		}
		m.mu.Lock()
		f, err := m.roundtrip(ctx, m.complete, &pending, resMeta, payload)
		m.mu.Unlock()
		if err != nil {
			return nil, nil, err
		}
		if f != nil {
			frame = f
		}
	}
	metaLen := binary.BigEndian.Uint32(frame[:4])
	var res responseMeta
	if err := json.Unmarshal(frame[4:4+metaLen], &res); err != nil {
		return nil, nil, fmt.Errorf("wasmo: decode response meta: %w", err)
	}
	if res.Await != "" {
		return nil, nil, fmt.Errorf("wasmo: invocation %s was never answered", res.Await)
	}
	return &res, frame[4+metaLen:], nil
}

func (m *Mock) roundtrip(ctx context.Context, fn api.Function, pending *[]invocation, meta, body []byte) ([]byte, error) {
	metaPtr, err := m.put(ctx, meta)
	if err != nil {
		return nil, err
	}
	bodyPtr, err := m.put(ctx, body)
	if err != nil {
		return nil, err
	}
	res, err := fn.Call(context.WithValue(ctx, queueKey{}, pending), uint64(metaPtr), uint64(len(meta)), uint64(bodyPtr), uint64(len(body)))
	if err != nil {
		return nil, err
	}
	if res[0] == 0 {
		return nil, nil
	}
	ptr, length := uint32(res[0]>>32), uint32(res[0])
	out, ok := m.module.Memory().Read(ptr, length)
	if !ok {
		return nil, fmt.Errorf("wasmo: response out of range")
	}
	frame := append([]byte(nil), out...)
	if _, err := m.free.Call(ctx, uint64(ptr), uint64(length)); err != nil {
		return nil, err
	}
	return frame, nil
}

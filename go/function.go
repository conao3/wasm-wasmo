package wasmo

import (
	"context"
	"encoding/json"
	"fmt"
	"reflect"
	"runtime/debug"
	"strings"
	"sync"
	"time"
)

// Handler runs a Lambda function. The event is the invocation payload; the
// returned value is the function's response and a non-nil error becomes a
// function error (X-Amz-Function-Error: Unhandled).
type Handler func(ctx context.Context, event json.RawMessage) (json.RawMessage, error)

// FunctionContext describes the invocation a Handler runs, as the Lambda
// context object does. FunctionContextFrom reads it from the handler's ctx.
type FunctionContext struct {
	FunctionName       string
	FunctionVersion    string
	InvokedFunctionArn string
	AwsRequestID       string
	MemoryLimitInMB    int
	LogGroupName       string
	LogStreamName      string
	InvocationType     string
	ClientContext      string
	Deadline           time.Time
	// Environment holds the function's environment variables and the
	// variables Lambda sets (AWS_REGION, AWS_LAMBDA_FUNCTION_NAME, ...).
	Environment map[string]string

	mu  sync.Mutex
	log strings.Builder
}

// Logf appends a line to the invocation's log, which Invoke returns with
// LogType Tail.
func (c *FunctionContext) Logf(format string, args ...any) {
	c.mu.Lock()
	defer c.mu.Unlock()
	line := fmt.Sprintf(format, args...)
	c.log.WriteString(line)
	if !strings.HasSuffix(line, "\n") {
		c.log.WriteByte('\n')
	}
}

type functionContextKey struct{}

// FunctionContextFrom returns the context of the invocation a Handler runs.
func FunctionContextFrom(ctx context.Context) (*FunctionContext, bool) {
	c, ok := ctx.Value(functionContextKey{}).(*FunctionContext)
	return c, ok
}

// RegisterFunction binds a handler to every invocation of the Lambda function
// with this name, whatever code CreateFunction was given. The handler runs
// outside the mock's lock, so it may call AWS through the same Mock.
func (m *Mock) RegisterFunction(name string, h Handler) {
	m.fnMu.Lock()
	defer m.fnMu.Unlock()
	if m.functions == nil {
		m.functions = map[string]Handler{}
	}
	m.functions[name] = h
}

type invocation struct {
	meta    []byte
	payload []byte
}

type invocationMeta struct {
	ID                 string            `json:"id"`
	FunctionName       string            `json:"functionName"`
	FunctionVersion    string            `json:"functionVersion"`
	InvokedFunctionArn string            `json:"invokedFunctionArn"`
	MemoryLimitInMB    int               `json:"memoryLimitInMB"`
	DeadlineMs         float64           `json:"deadlineMs"`
	AwsRequestID       string            `json:"awsRequestId"`
	LogGroupName       string            `json:"logGroupName"`
	LogStreamName      string            `json:"logStreamName"`
	InvocationType     string            `json:"invocationType"`
	ClientContext      string            `json:"clientContext"`
	Environment        map[string]string `json:"environment"`
}

type functionError struct {
	Type       string   `json:"type"`
	Message    string   `json:"message"`
	StackTrace []string `json:"stackTrace,omitempty"`
}

type resultMeta struct {
	ID         string         `json:"id"`
	Registered bool           `json:"registered"`
	Error      *functionError `json:"error,omitempty"`
	Log        string         `json:"log"`
}

type queueKey struct{}

func errorType(err error) string {
	t := reflect.TypeOf(err)
	if t.Kind() == reflect.Pointer {
		t = t.Elem()
	}
	return t.Name()
}

// run executes one invocation the module announced and encodes the result
// meta and payload for wasmo_complete.
func (m *Mock) run(ctx context.Context, inv invocation) ([]byte, []byte, error) {
	var meta invocationMeta
	if err := json.Unmarshal(inv.meta, &meta); err != nil {
		return nil, nil, fmt.Errorf("wasmo: decode invocation: %w", err)
	}
	m.fnMu.Lock()
	h, ok := m.functions[meta.FunctionName]
	m.fnMu.Unlock()
	res := resultMeta{ID: meta.ID, Registered: ok}
	var payload []byte
	if ok {
		fc := &FunctionContext{
			FunctionName:       meta.FunctionName,
			FunctionVersion:    meta.FunctionVersion,
			InvokedFunctionArn: meta.InvokedFunctionArn,
			AwsRequestID:       meta.AwsRequestID,
			MemoryLimitInMB:    meta.MemoryLimitInMB,
			LogGroupName:       meta.LogGroupName,
			LogStreamName:      meta.LogStreamName,
			InvocationType:     meta.InvocationType,
			ClientContext:      meta.ClientContext,
			Deadline:           time.UnixMilli(int64(meta.DeadlineMs)),
			Environment:        meta.Environment,
		}
		hctx, cancel := context.WithDeadline(context.WithValue(ctx, functionContextKey{}, fc), fc.Deadline)
		out, err := call(hctx, h, inv.payload)
		cancel()
		if err != nil {
			res.Error = &functionError{Type: errorType(err), Message: err.Error()}
			if p, ok := err.(panicError); ok {
				res.Error = &functionError{Type: p.kind, Message: p.message, StackTrace: p.stack}
			}
		} else {
			payload = out
		}
		fc.mu.Lock()
		res.Log = fc.log.String()
		fc.mu.Unlock()
	}
	b, err := json.Marshal(res)
	return b, payload, err
}

type panicError struct {
	kind    string
	message string
	stack   []string
}

func (p panicError) Error() string { return p.message }

func call(ctx context.Context, h Handler, event []byte) (out json.RawMessage, err error) {
	defer func() {
		if r := recover(); r != nil {
			err = panicError{
				kind:    reflect.TypeOf(r).String(),
				message: fmt.Sprint(r),
				stack:   strings.Split(strings.TrimSpace(string(debug.Stack())), "\n"),
			}
		}
	}()
	return h(ctx, event)
}

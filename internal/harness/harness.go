// Package harness abstracts the LLM call used by every swarm worker.
//
// A Harness is the single seam between the orchestrator and a language model:
// the runner only ever asks for "complete this conversation" and gets back text
// plus usage. That keeps the swarm testable without network access
// (EchoHarness) and lets production swap in an OpenAI-compatible endpoint
// (DirectHarness) without touching call sites.
//
// Two guards keep a fleet of concurrent workers from melting the machine or
// the upstream provider's rate limits:
//
//   - Semaphore caps how many calls are in flight at once.
//   - Limiter is a token bucket capping how many calls start per interval.
//
// Both are context aware, so a cancelled turn never leaves a blocked worker (or
// a leaked semaphore slot) behind. Limited composes the two around any
// Harness.
//
// The package has no third-party dependencies.
package harness

import (
	"context"
	"errors"
	"fmt"
	"strings"
)

// Role identifies who authored a message.
type Role string

// The three roles every chat-completion API understands.
const (
	RoleSystem    Role = "system"
	RoleUser      Role = "user"
	RoleAssistant Role = "assistant"
)

// Defaults used when a config field is left at its zero value.
const (
	DefaultMaxTokens    = 1024
	DefaultMaxRetries   = 2
	DefaultTimeoutMs    = 120000
	DefaultRetryBackoff = 250 // milliseconds between retries
	DefaultBaseURL      = "https://api.openai.com"
	DefaultChatPath     = "/v1/chat/completions"
)

// Sentinel errors. Callers branch on these with errors.Is instead of parsing
// message text.
var (
	// ErrEmptyConversation means the request carried no messages at all.
	ErrEmptyConversation = errors.New("harness: request has no messages")
	// ErrBadRole means a message used a role the provider does not accept.
	ErrBadRole = errors.New("harness: invalid message role")
	// ErrNoEndpoint means DirectConfig is missing a base URL.
	ErrNoEndpoint = errors.New("harness: direct harness needs a base url")
	// ErrNoModel means neither the config nor the request named a model.
	ErrNoModel = errors.New("harness: no model configured")
	// ErrEmptyCompletion means the provider answered 200 with no text.
	ErrEmptyCompletion = errors.New("harness: provider returned an empty completion")
	// ErrNilHarness means a wrapper was built around a nil Harness.
	ErrNilHarness = errors.New("harness: nil harness")
)

// Message is one turn of the conversation.
type Message struct {
	Role    Role   `json:"role"`
	Content string `json:"content"`
}

// Valid reports whether m.Role is one of the three supported roles.
func (m Message) Valid() bool {
	switch m.Role {
	case RoleSystem, RoleUser, RoleAssistant:
		return true
	default:
		return false
	}
}

// Request is a single completion request. It is deliberately plain data so it
// can be logged, cached or replayed verbatim.
type Request struct {
	Model       string    `json:"model,omitempty"`
	Messages    []Message `json:"messages"`
	MaxTokens   int       `json:"maxTokens,omitempty"`
	Temperature float64   `json:"temperature,omitempty"`
	// Stop lists sequences that end the completion early.
	Stop []string `json:"stop,omitempty"`
}

// Validate checks the request is well formed and returns the matching sentinel
// error otherwise.
func (r Request) Validate() error {
	if len(r.Messages) == 0 {
		return ErrEmptyConversation
	}
	for i, m := range r.Messages {
		if !m.Valid() {
			return fmt.Errorf("%w: message %d has role %q", ErrBadRole, i, m.Role)
		}
	}
	return nil
}

// LastUser returns the content of the final user message, or "" when the
// conversation contains no user turn.
func (r Request) LastUser() string {
	for i := len(r.Messages) - 1; i >= 0; i-- {
		if r.Messages[i].Role == RoleUser {
			return r.Messages[i].Content
		}
	}
	return ""
}

// Normalize returns a copy with MaxTokens defaulted.
func (r Request) Normalize() Request {
	if r.MaxTokens <= 0 {
		r.MaxTokens = DefaultMaxTokens
	}
	return r
}

// Usage reports token accounting as returned by the provider.
type Usage struct {
	PromptTokens     int `json:"promptTokens"`
	CompletionTokens int `json:"completionTokens"`
	TotalTokens      int `json:"totalTokens"`
}

// Response is the result of a completion.
type Response struct {
	Text       string `json:"text"`
	Model      string `json:"model,omitempty"`
	StopReason string `json:"stopReason,omitempty"`
	Usage      Usage  `json:"usage"`
	// Attempts is how many round trips it took (1 when not retried).
	Attempts int `json:"attempts,omitempty"`
}

// Harness completes a conversation. Implementations must be safe for
// concurrent use.
type Harness interface {
	Complete(ctx context.Context, req Request) (Response, error)
}

// HarnessFunc adapts a plain function to the Harness interface.
type HarnessFunc func(ctx context.Context, req Request) (Response, error)

// Complete implements Harness.
func (f HarnessFunc) Complete(ctx context.Context, req Request) (Response, error) {
	return f(ctx, req)
}

// EchoHarness is the offline harness: it never touches the network and answers
// deterministically with the last user message.
type EchoHarness struct {
	// Model is reported back in Response.Model (defaults to "echo").
	Model string
	// Prefix is prepended to the echoed text (defaults to "echo: ").
	Prefix string
}

// NewEcho returns an EchoHarness with the standard prefix.
func NewEcho() *EchoHarness { return &EchoHarness{Model: "echo", Prefix: "echo: "} }

// Complete implements Harness.
func (e *EchoHarness) Complete(ctx context.Context, req Request) (Response, error) {
	if err := ctx.Err(); err != nil {
		return Response{}, err
	}
	if err := req.Validate(); err != nil {
		return Response{}, err
	}
	model := e.Model
	if model == "" {
		model = "echo"
	}
	text := e.Prefix + strings.TrimSpace(req.LastUser())
	prompt := CountTokens(req)
	completion := len(strings.Fields(text))
	return Response{
		Text:     text,
		Model:    model,
		Attempts: 1,
		Usage: Usage{
			PromptTokens:     prompt,
			CompletionTokens: completion,
			TotalTokens:      prompt + completion,
		},
	}, nil
}

// CountTokens is a cheap whitespace-based estimate used by the offline harness
// and for logging when a provider omits usage numbers.
func CountTokens(req Request) int {
	n := 0
	for _, m := range req.Messages {
		n += len(strings.Fields(m.Content))
	}
	return n
}

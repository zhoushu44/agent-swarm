package harness

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"
)

// DefaultTimeout is how long a single HTTP round trip may take before it is
// abandoned and (possibly) retried.
const DefaultTimeout = DefaultTimeoutMs * time.Millisecond

// DirectConfig configures DirectHarness. Zero values fall back to the Default*
// constants; only Model (here or on Request) is mandatory.
type DirectConfig struct {
	// BaseURL is the API origin, e.g. "https://api.openai.com".
	BaseURL string
	// ChatPath is appended to BaseURL (default DefaultChatPath).
	ChatPath string
	// APIKey is sent as a bearer token when non-empty.
	APIKey string
	// Model is the default model used when Request.Model is empty.
	Model string
	// Timeout bounds a single round trip (default DefaultTimeout).
	Timeout time.Duration
	// MaxRetries is how many times a retryable failure is retried (default
	// DefaultMaxRetries). A negative value disables retries.
	MaxRetries int
	// RetryBackoff is the base delay between retries; it grows linearly with
	// the attempt number (default DefaultRetryBackoff).
	RetryBackoff time.Duration
	// Client overrides the HTTP client (tests inject an httptest client).
	Client *http.Client
	// ExtraHeaders are added to every request (org ids, custom auth, ...).
	ExtraHeaders map[string]string
}

// Normalize returns a copy of c with unset fields defaulted.
func (c DirectConfig) Normalize() DirectConfig {
	if c.BaseURL == "" {
		c.BaseURL = DefaultBaseURL
	}
	c.BaseURL = strings.TrimRight(c.BaseURL, "/")
	if c.ChatPath == "" {
		// Mirror the JS harness: if the base already ends in /v1 the chat path
		// must be /chat/completions, not /v1/chat/completions.
		if strings.HasSuffix(c.BaseURL, "/v1") {
			c.ChatPath = "/chat/completions"
		} else {
			c.ChatPath = DefaultChatPath
		}
	}
	if !strings.HasPrefix(c.ChatPath, "/") {
		c.ChatPath = "/" + c.ChatPath
	}
	if c.Timeout <= 0 {
		c.Timeout = DefaultTimeout
	}
	if c.MaxRetries < 0 {
		c.MaxRetries = 0
	}
	if c.RetryBackoff <= 0 {
		c.RetryBackoff = DefaultRetryBackoff * time.Millisecond
	}
	if c.Client == nil {
		c.Client = &http.Client{Timeout: c.Timeout}
	}
	return c
}

// DirectHarness talks to an OpenAI-compatible /chat/completions endpoint over
// plain net/http. It retries transport errors, 429 and 5xx responses; it never
// retries a 4xx (the request itself is wrong).
type DirectHarness struct {
	cfg   DirectConfig
	sleep func(context.Context, time.Duration) error
}

// NewDirect normalizes cfg and returns a DirectHarness.
func NewDirect(cfg DirectConfig) (*DirectHarness, error) {
	norm := cfg.Normalize()
	if norm.BaseURL == "" {
		return nil, ErrNoEndpoint
	}
	return &DirectHarness{cfg: norm, sleep: sleepCtx}, nil
}

// Config returns the normalized configuration (useful for logging).
func (d *DirectHarness) Config() DirectConfig { return d.cfg }

// wire types mirroring the OpenAI chat-completions schema.
type chatRequest struct {
	Model       string    `json:"model"`
	Messages    []Message `json:"messages"`
	MaxTokens   int       `json:"max_tokens,omitempty"`
	Temperature float64   `json:"temperature,omitempty"`
	Stop        []string  `json:"stop,omitempty"`
}

type chatResponse struct {
	Model   string `json:"model"`
	Choices []struct {
		Message      Message `json:"message"`
		FinishReason string  `json:"finish_reason"`
	} `json:"choices"`
	Usage struct {
		PromptTokens     int `json:"prompt_tokens"`
		CompletionTokens int `json:"completion_tokens"`
		TotalTokens      int `json:"total_tokens"`
	} `json:"usage"`
}

// Complete implements Harness.
func (d *DirectHarness) Complete(ctx context.Context, req Request) (Response, error) {
	if d == nil {
		return Response{}, ErrNilHarness
	}
	if err := req.Validate(); err != nil {
		return Response{}, err
	}
	req = req.Normalize()
	model := req.Model
	if model == "" {
		model = d.cfg.Model
	}
	if model == "" {
		return Response{}, ErrNoModel
	}

	body, err := json.Marshal(chatRequest{
		Model:       model,
		Messages:    req.Messages,
		MaxTokens:   req.MaxTokens,
		Temperature: req.Temperature,
		Stop:        req.Stop,
	})
	if err != nil {
		return Response{}, fmt.Errorf("harness: encode request: %w", err)
	}

	url := d.cfg.BaseURL + d.cfg.ChatPath
	var lastErr error
	for attempt := 0; attempt <= d.cfg.MaxRetries; attempt++ {
		if attempt > 0 {
			if err := d.sleep(ctx, time.Duration(attempt)*d.cfg.RetryBackoff); err != nil {
				return Response{}, err
			}
		}
		resp, retryable, err := d.roundTrip(ctx, url, body)
		if err == nil {
			resp.Attempts = attempt + 1
			return resp, nil
		}
		lastErr = err
		if !retryable || ctx.Err() != nil {
			return Response{}, err
		}
	}
	return Response{}, fmt.Errorf("harness: giving up after %d attempts: %w", d.cfg.MaxRetries+1, lastErr)
}

// roundTrip performs one HTTP exchange. retryable reports whether the failure
// is worth another attempt.
func (d *DirectHarness) roundTrip(ctx context.Context, url string, body []byte) (Response, bool, error) {
	httpReq, err := http.NewRequestWithContext(ctx, http.MethodPost, url, bytes.NewReader(body))
	if err != nil {
		return Response{}, false, fmt.Errorf("harness: build request: %w", err)
	}
	httpReq.Header.Set("Content-Type", "application/json")
	httpReq.Header.Set("Accept", "application/json")
	if d.cfg.APIKey != "" {
		httpReq.Header.Set("Authorization", "Bearer "+d.cfg.APIKey)
	}
	for k, v := range d.cfg.ExtraHeaders {
		httpReq.Header.Set(k, v)
	}

	httpResp, err := d.cfg.Client.Do(httpReq)
	if err != nil {
		return Response{}, true, fmt.Errorf("harness: transport: %w", err)
	}
	defer httpResp.Body.Close()

	payload, err := io.ReadAll(io.LimitReader(httpResp.Body, 8<<20))
	if err != nil {
		return Response{}, true, fmt.Errorf("harness: read body: %w", err)
	}

	if httpResp.StatusCode < 200 || httpResp.StatusCode >= 300 {
		httpErr := &HTTPError{
			StatusCode: httpResp.StatusCode,
			Status:     httpResp.Status,
			Body:       truncate(string(payload), 512),
		}
		return Response{}, httpErr.Retryable(), httpErr
	}

	var parsed chatResponse
	if err := json.Unmarshal(payload, &parsed); err != nil {
		return Response{}, false, fmt.Errorf("harness: decode response: %w", err)
	}
	if len(parsed.Choices) == 0 {
		return Response{}, false, ErrEmptyCompletion
	}
	choice := parsed.Choices[0]
	usage := Usage{
		PromptTokens:     parsed.Usage.PromptTokens,
		CompletionTokens: parsed.Usage.CompletionTokens,
		TotalTokens:      parsed.Usage.TotalTokens,
	}
	if usage.TotalTokens == 0 {
		usage.TotalTokens = usage.PromptTokens + usage.CompletionTokens
	}
	model := parsed.Model
	if model == "" {
		model = d.cfg.Model
	}
	return Response{
		Text:       choice.Message.Content,
		Model:      model,
		StopReason: choice.FinishReason,
		Usage:      usage,
	}, false, nil
}

// HTTPError is returned for a non-2xx provider response.
type HTTPError struct {
	StatusCode int
	Status     string
	Body       string
}

// Error implements error.
func (e *HTTPError) Error() string {
	return fmt.Sprintf("harness: provider status %s: %s", e.Status, e.Body)
}

// Retryable reports whether the status warrants another attempt.
func (e *HTTPError) Retryable() bool {
	return e.StatusCode == http.StatusTooManyRequests || e.StatusCode >= 500
}

func truncate(s string, n int) string {
	s = strings.TrimSpace(s)
	if len(s) <= n {
		return s
	}
	return s[:n] + "..."
}

// sleepCtx waits for d or until ctx is cancelled, whichever comes first.
func sleepCtx(ctx context.Context, d time.Duration) error {
	if d <= 0 {
		return ctx.Err()
	}
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-t.C:
		return nil
	}
}

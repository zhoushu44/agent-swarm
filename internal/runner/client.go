// Package runner drives one swarm worker: it claims an item from the swarm
// HTTP API, runs a turn, and submits the resulting commit.
//
// This file is only the transport half: a small, typed HTTP client for the
// swarm API. It mirrors the routes of src/server.js and reuses the wire types
// from internal/protocol, and it depends on nothing outside the standard
// library, so it can be exercised against net/http/httptest without touching a
// real server or a real database.
//
// The client deliberately returns typed errors: callers must be able to tell a
// lost race (409) from a missing item (404) without parsing message text,
// because the runner's whole job is to react to those two cases differently.
package runner

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"time"

	"swarm-go/internal/protocol"
)

// Defaults applied by NewClient.
const (
	// DefaultBaseURL is the address a local swarm server listens on.
	DefaultBaseURL = "http://127.0.0.1:17771"
	// DefaultTimeout bounds a single API call. A hung API must never wedge a
	// worker turn: the runner turns the timeout into a retry, not a lost claim.
	DefaultTimeout = 30 * time.Second
	// DefaultBoardLimit matches the server-side default page size.
	DefaultBoardLimit = 200
	// maxErrorBody caps how much of an error response is kept in memory.
	maxErrorBody = 64 << 10
)

// Sentinel errors. Every failed call wraps exactly one of these, so callers can
// branch with errors.Is(err, runner.ErrConflict) instead of inspecting status
// codes by hand.
var (
	// ErrConflict means the item is already claimed or terminal (HTTP 409).
	ErrConflict = errors.New("swarm api: conflict")
	// ErrNotFound means the item or claim token does not exist (HTTP 404).
	ErrNotFound = errors.New("swarm api: not found")
	// ErrBadRequest means the request was rejected as malformed (HTTP 400).
	ErrBadRequest = errors.New("swarm api: bad request")
	// ErrForbidden means the caller does not own the claim (HTTP 403).
	ErrForbidden = errors.New("swarm api: forbidden")
	// ErrUnavailable means the server failed or could not be reached (5xx,
	// transport error, timeout). These are worth retrying; the others are not.
	ErrUnavailable = errors.New("swarm api: unavailable")
)

// APIError is the error returned by every Client method. It carries the HTTP
// status, the server's error message (when it sent one) and wraps the matching
// sentinel so errors.Is works.
//
// An APIError with StatusCode 0 means the request never produced a response
// (connection refused, timeout, cancelled context).
type APIError struct {
	Method     string // HTTP method
	Path       string // request path, without the base URL
	StatusCode int    // 0 when no response was received
	Message    string // server error text, when available
	Err        error  // sentinel, or the transport error for StatusCode 0
}

// Error implements error.
func (e *APIError) Error() string {
	switch {
	case e.StatusCode == 0:
		return fmt.Sprintf("%s %s: %v", e.Method, e.Path, e.Err)
	case e.Message != "":
		return fmt.Sprintf("%s %s: %d: %s", e.Method, e.Path, e.StatusCode, e.Message)
	default:
		return fmt.Sprintf("%s %s: %d", e.Method, e.Path, e.StatusCode)
	}
}

// Unwrap exposes the sentinel (or transport error) to errors.Is / errors.As.
func (e *APIError) Unwrap() error { return e.Err }

// Retryable reports whether the failure is worth another attempt.
func (e *APIError) Retryable() bool {
	return e.StatusCode == 0 || e.StatusCode >= 500
}

// IsRetryable reports whether err is a transient API failure.
func IsRetryable(err error) bool {
	var apiErr *APIError
	if errors.As(err, &apiErr) {
		return apiErr.Retryable()
	}
	return false
}

// IsConflict reports whether err is a lost race for an item.
func IsConflict(err error) bool { return errors.Is(err, ErrConflict) }

// IsNotFound reports whether err is a missing item or token.
func IsNotFound(err error) bool { return errors.Is(err, ErrNotFound) }

// ClaimResult is the task package returned by POST /claim: the lease token, the
// item detail and the item's past findings/failures.
type ClaimResult struct {
	Token   string                   `json:"token"`
	Item    *protocol.Item           `json:"item"`
	History []*protocol.ContextEntry `json:"history"`
}

// Client talks to the swarm HTTP API.
//
// A Client is safe for concurrent use and is meant to be shared by every
// worker in a process.
type Client struct {
	baseURL string
	http    *http.Client
	peer    string // default peer name attached to peer-scoped helpers
}

// NewClient returns a Client for the swarm API at baseURL. An empty baseURL
// falls back to DefaultBaseURL; a nil httpClient gets a client with
// DefaultTimeout.
func NewClient(baseURL string, httpClient *http.Client) *Client {
	base := strings.TrimSpace(baseURL)
	if base == "" {
		base = DefaultBaseURL
	}
	base = strings.TrimRight(base, "/")
	if httpClient == nil {
		httpClient = &http.Client{Timeout: DefaultTimeout}
	}
	return &Client{baseURL: base, http: httpClient}
}

// BaseURL returns the API root this client talks to.
func (c *Client) BaseURL() string { return c.baseURL }

// SetPeer sets the default peer name used by peer-scoped helpers (Digest,
// Messages). It is optional: every explicit call still takes its own peer.
func (c *Client) SetPeer(peer string) { c.peer = strings.TrimSpace(peer) }

// Peer returns the default peer name.
func (c *Client) Peer() string { return c.peer }

// do performs one API call: it JSON-encodes body (when non-nil), decodes a 2xx
// response into out (when non-nil) and maps any other outcome onto APIError.
func (c *Client) do(ctx context.Context, method, path string, query url.Values, body, out any) error {
	u := c.baseURL + path
	if len(query) > 0 {
		u += "?" + query.Encode()
	}

	var reader io.Reader
	if body != nil {
		buf, err := json.Marshal(body)
		if err != nil {
			return &APIError{Method: method, Path: path, Err: fmt.Errorf("%w: encode body: %v", ErrBadRequest, err)}
		}
		reader = bytes.NewReader(buf)
	}

	req, err := http.NewRequestWithContext(ctx, method, u, reader)
	if err != nil {
		return &APIError{Method: method, Path: path, Err: fmt.Errorf("%w: %v", ErrBadRequest, err)}
	}
	req.Header.Set("Accept", "application/json")
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}

	resp, err := c.http.Do(req)
	if err != nil {
		return &APIError{Method: method, Path: path, Err: fmt.Errorf("%w: %v", ErrUnavailable, err)}
	}
	defer resp.Body.Close()

	if resp.StatusCode < 200 || resp.StatusCode > 299 {
		return &APIError{
			Method:     method,
			Path:       path,
			StatusCode: resp.StatusCode,
			Message:    errorMessage(resp.Body),
			Err:        sentinelFor(resp.StatusCode),
		}
	}
	if out == nil {
		_, _ = io.Copy(io.Discard, io.LimitReader(resp.Body, maxErrorBody))
		return nil
	}
	if err := json.NewDecoder(resp.Body).Decode(out); err != nil {
		return &APIError{Method: method, Path: path, StatusCode: resp.StatusCode,
			Err: fmt.Errorf("%w: decode response: %v", ErrUnavailable, err)}
	}
	return nil
}

// errorMessage extracts the {"error": "..."} text from a failed response,
// falling back to the trimmed raw body so nothing is lost for a human reader.
func errorMessage(body io.Reader) string {
	raw, _ := io.ReadAll(io.LimitReader(body, maxErrorBody))
	var payload struct {
		Error string `json:"error"`
	}
	if json.Unmarshal(raw, &payload) == nil && strings.TrimSpace(payload.Error) != "" {
		return strings.TrimSpace(payload.Error)
	}
	return strings.TrimSpace(string(raw))
}

// sentinelFor maps an HTTP status onto the sentinel callers branch on.
func sentinelFor(status int) error {
	switch status {
	case http.StatusBadRequest:
		return ErrBadRequest
	case http.StatusForbidden:
		return ErrForbidden
	case http.StatusNotFound:
		return ErrNotFound
	case http.StatusConflict:
		return ErrConflict
	default:
		if status >= 500 {
			return ErrUnavailable
		}
		return ErrBadRequest
	}
}

// itoa is a tiny helper so query building stays readable.
func itoa(n int) string { return strconv.Itoa(n) }

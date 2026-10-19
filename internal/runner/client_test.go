package runner

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

// recorded captures the last request the fake server saw.
type recorded struct {
	method string
	path   string
	query  string
	body   map[string]any
}

// newTestClient starts a fake swarm API that always answers with status/body
// and returns a Client pointed at it plus the request recorder.
func newTestClient(t *testing.T, status int, body string) (*Client, *recorded) {
	t.Helper()
	rec := &recorded{}
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		rec.method = r.Method
		rec.path = r.URL.Path
		rec.query = r.URL.RawQuery
		if r.Body != nil {
			raw, _ := io.ReadAll(r.Body)
			if len(raw) > 0 {
				_ = json.Unmarshal(raw, &rec.body)
			}
		}
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(status)
		_, _ = io.WriteString(w, body)
	}))
	t.Cleanup(srv.Close)
	return NewClient(srv.URL, srv.Client()), rec
}

func TestNewClientDefaults(t *testing.T) {
	c := NewClient("", nil)
	if got, want := c.BaseURL(), DefaultBaseURL; got != want {
		t.Fatalf("BaseURL() = %q, want %q", got, want)
	}
	if got, want := NewClient("http://example.test/", nil).BaseURL(), "http://example.test"; got != want {
		t.Fatalf("trailing slash not trimmed: %q", got)
	}
	c.SetPeer(" w2 ")
	if got, want := c.Peer(), "w2"; got != want {
		t.Fatalf("Peer() = %q, want %q", got, want)
	}
}

func TestDoMapsStatusToSentinel(t *testing.T) {
	cases := []struct {
		status int
		want   error
	}{
		{http.StatusBadRequest, ErrBadRequest},
		{http.StatusForbidden, ErrForbidden},
		{http.StatusNotFound, ErrNotFound},
		{http.StatusConflict, ErrConflict},
		{http.StatusInternalServerError, ErrUnavailable},
		{http.StatusServiceUnavailable, ErrUnavailable},
		{http.StatusTeapot, ErrBadRequest},
	}
	for _, tc := range cases {
		t.Run(http.StatusText(tc.status), func(t *testing.T) {
			c, _ := newTestClient(t, tc.status, `{"error":"boom"}`)
			_, err := c.PendingBoard(context.Background())
			if !errors.Is(err, tc.want) {
				t.Fatalf("err = %v, want %v", err, tc.want)
			}
			var apiErr *APIError
			if !errors.As(err, &apiErr) {
				t.Fatalf("err = %v, want *APIError", err)
			}
			if apiErr.StatusCode != tc.status || apiErr.Message != "boom" {
				t.Fatalf("apiErr = %+v", apiErr)
			}
			if !strings.Contains(apiErr.Error(), "boom") {
				t.Fatalf("Error() = %q", apiErr.Error())
			}
		})
	}
}

func TestErrorBodyFallsBackToRawText(t *testing.T) {
	c, _ := newTestClient(t, http.StatusConflict, `not json at all`)
	_, err := c.PendingBoard(context.Background())
	if !IsConflict(err) {
		t.Fatalf("err = %v, want conflict", err)
	}
	var apiErr *APIError
	if !errors.As(err, &apiErr) || apiErr.Message != "not json at all" {
		t.Fatalf("apiErr = %+v", apiErr)
	}
}

func TestTransportErrorIsUnavailableAndRetryable(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {}))
	url := srv.URL
	srv.Close() // nothing is listening any more

	c := NewClient(url, srv.Client())
	_, err := c.PendingBoard(context.Background())
	if !errors.Is(err, ErrUnavailable) {
		t.Fatalf("err = %v, want ErrUnavailable", err)
	}
	if !IsRetryable(err) {
		t.Fatalf("transport failure must be retryable: %v", err)
	}
	var apiErr *APIError
	if !errors.As(err, &apiErr) || apiErr.StatusCode != 0 {
		t.Fatalf("apiErr = %+v", apiErr)
	}
}

func TestTimeoutIsUnavailable(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		time.Sleep(200 * time.Millisecond)
		_, _ = io.WriteString(w, `{}`)
	}))
	defer srv.Close()

	c := NewClient(srv.URL, &http.Client{Timeout: 20 * time.Millisecond})
	_, err := c.PendingBoard(context.Background())
	if !IsRetryable(err) {
		t.Fatalf("timeout must be retryable: %v", err)
	}
}

func TestBadJSONResponseIsUnavailable(t *testing.T) {
	c, _ := newTestClient(t, http.StatusOK, `{"items":`)
	if _, err := c.PendingBoard(context.Background()); !errors.Is(err, ErrUnavailable) {
		t.Fatalf("err = %v, want ErrUnavailable", err)
	}
}

func TestIsRetryableIgnoresPlainErrors(t *testing.T) {
	if IsRetryable(nil) || IsRetryable(errors.New("nope")) {
		t.Fatal("non-API errors must not be retryable")
	}
	if IsConflict(errors.New("nope")) || IsNotFound(errors.New("nope")) {
		t.Fatal("plain errors must not match sentinels")
	}
}

func TestAPIErrorStringForms(t *testing.T) {
	noBody := &APIError{Method: "GET", Path: "/board", StatusCode: 500}
	if got, want := noBody.Error(), "GET /board: 500"; got != want {
		t.Fatalf("Error() = %q, want %q", got, want)
	}
	transport := &APIError{Method: "GET", Path: "/board", Err: ErrUnavailable}
	if got := transport.Error(); !strings.Contains(got, "unavailable") {
		t.Fatalf("Error() = %q", got)
	}
}

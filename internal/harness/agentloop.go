package harness

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

// AgentLoopConfig tunes the agent tool-calling loop.
type AgentLoopConfig struct {
	// WorkDir is the working directory for bash/write (typically the worker worktree).
	WorkDir string
	// ServerPort is the local swarm server port the http tool targets.
	ServerPort int
	// MaxSteps caps tool-call rounds (default 40).
	MaxSteps int
	// BashTimeout bounds one bash command (default 60s).
	BashTimeout time.Duration
}

func (c AgentLoopConfig) normalize() AgentLoopConfig {
	if c.MaxSteps <= 0 {
		c.MaxSteps = 40
	}
	if c.BashTimeout <= 0 {
		c.BashTimeout = 60 * time.Second
	}
	return c
}

// tool wire types (OpenAI function-calling schema).
type toolDef struct {
	Type     string      `json:"type"`
	Function toolFuncDef `json:"function"`
}
type toolFuncDef struct {
	Name        string          `json:"name"`
	Description string          `json:"description"`
	Parameters  json.RawMessage `json:"parameters"`
}

type toolCall struct {
	ID       string `json:"id"`
	Type     string `json:"type"`
	Function struct {
		Name      string `json:"name"`
		Arguments string `json:"arguments"`
	} `json:"function"`
}

// loopMessage extends Message with tool-calling fields.
type loopMessage struct {
	Role       string     `json:"role"`
	Content    string     `json:"content,omitempty"`
	ToolCalls  []toolCall `json:"tool_calls,omitempty"`
	ToolCallID string     `json:"tool_call_id,omitempty"`
	Name       string     `json:"name,omitempty"`
}

type loopChatRequest struct {
	Model       string        `json:"model"`
	Messages    []loopMessage `json:"messages"`
	Tools       []toolDef     `json:"tools,omitempty"`
	ToolChoice  string        `json:"tool_choice,omitempty"`
	MaxTokens   int           `json:"max_tokens,omitempty"`
	Temperature float64       `json:"temperature,omitempty"`
}

type loopChatResponse struct {
	Model   string `json:"model"`
	Choices []struct {
		Message struct {
			Content   string     `json:"content"`
			ToolCalls []toolCall `json:"tool_calls"`
		} `json:"message"`
		FinishReason string `json:"finish_reason"`
	} `json:"choices"`
	Usage struct {
		PromptTokens     int `json:"prompt_tokens"`
		CompletionTokens int `json:"completion_tokens"`
		TotalTokens      int `json:"total_tokens"`
	} `json:"usage"`
}

// AgentLoopResult is the outcome of a tool-calling agent loop.
type AgentLoopResult struct {
	Reply     string
	Usage     Usage
	Steps     int
	Submitted bool // true when the http tool posted /submit successfully
}

// swarmTools returns the four tool schemas (bash/write/http/finish) mirroring
// the JS harness. Schemas use single-string JSON payloads where the upstream
// proxy drops nested objects.
func swarmTools() []toolDef {
	mustJSON := func(s string) json.RawMessage { return json.RawMessage(s) }
	return []toolDef{
		{Type: "function", Function: toolFuncDef{
			Name:        "bash",
			Description: "Run a shell command in the worker worktree (cmd.exe on Windows). Returns stdout+stderr. 60s timeout.",
			Parameters:  mustJSON(`{"type":"object","properties":{"cmd":{"type":"string","description":"the shell command to run"}},"required":["cmd"]}`),
		}},
		{Type: "function", Function: toolFuncDef{
			Name:        "write",
			Description: "Write a file in the worktree. Input is a single JSON string field 'file' containing {\"path\":\"relative/path\",\"content\":\"...\"}. Path must stay inside the worktree.",
			Parameters:  mustJSON(`{"type":"object","properties":{"file":{"type":"string","description":"JSON string: {\"path\":\"...\",\"content\":\"...\"}"}},"required":["file"]}`),
		}},
		{Type: "function", Function: toolFuncDef{
			Name:        "http",
			Description: "Call the local swarm server API. Input is a single JSON string field 'request' containing {\"method\":\"GET|POST\",\"path\":\"/board|/claim|/submit|...\",\"...\":\"extra body/query fields\"}. Use it to claim items, submit work, read the board, post context.",
			Parameters:  mustJSON(`{"type":"object","properties":{"request":{"type":"string","description":"JSON string: {\"method\":\"POST\",\"path\":\"/claim\",\"itemId\":\"...\",\"peer\":\"...\"}"}},"required":["request"]}`),
		}},
		{Type: "function", Function: toolFuncDef{
			Name:        "finish",
			Description: "End this turn. Provide a one-line summary of what you did, or the word IDLE if there is no work for you right now.",
			Parameters:  mustJSON(`{"type":"object","properties":{"summary":{"type":"string","description":"one-line summary, or IDLE"}},"required":["summary"]}`),
		}},
	}
}

// RunAgentLoop drives an OpenAI tool-calling loop: the model may call
// bash/write/http/finish repeatedly until it calls finish, returns plain text,
// or MaxSteps is reached. Tool results are fed back as {role:"tool"} messages.
func (d *DirectHarness) RunAgentLoop(ctx context.Context, model, prompt string, cfg AgentLoopConfig) (AgentLoopResult, error) {
	if d == nil {
		return AgentLoopResult{}, ErrNilHarness
	}
	if model == "" {
		model = d.cfg.Model
	}
	if model == "" {
		return AgentLoopResult{}, ErrNoModel
	}
	cfg = cfg.normalize()

	messages := []loopMessage{{Role: "user", Content: prompt}}
	tools := swarmTools()
	url := d.cfg.BaseURL + d.cfg.ChatPath

	var total Usage
	var lastText string
	submitted := false
	// Some endpoints ignore tool_choice:"required" and answer with plain text
	// when they judge "no tool is needed" (observed on deepseek-v4-flash behind
	// certain proxies). That reply must NOT be treated as task completion when
	// nothing was submitted — otherwise the worker exits having done nothing.
	// Re-prime the model with a hard tool-use instruction instead, up to a cap
	// so a stubborn model cannot spin forever.
	plainTextRetries := 0
	const maxPlainTextRetries = 3

	for step := 0; step < cfg.MaxSteps; step++ {
		if err := ctx.Err(); err != nil {
			return AgentLoopResult{}, err
		}

		body, err := json.Marshal(loopChatRequest{
			Model:      model,
			Messages:   messages,
			Tools:      tools,
			ToolChoice: "required",
			MaxTokens:  4096,
		})
		if err != nil {
			return AgentLoopResult{}, fmt.Errorf("harness: encode agent request: %w", err)
		}

		resp, err := d.roundTripLoop(ctx, url, body)
		if err != nil {
			return AgentLoopResult{}, err
		}
		total.PromptTokens += resp.Usage.PromptTokens
		total.CompletionTokens += resp.Usage.CompletionTokens
		total.TotalTokens += resp.Usage.TotalTokens

		if len(resp.Choices) == 0 {
			return AgentLoopResult{}, ErrEmptyCompletion
		}
		msg := resp.Choices[0].Message
		if msg.Content != "" {
			lastText = msg.Content
		}

		// Append assistant turn (with tool_calls when present).
		messages = append(messages, loopMessage{
			Role:      "assistant",
			Content:   msg.Content,
			ToolCalls: msg.ToolCalls,
		})

		if len(msg.ToolCalls) == 0 {
			// Plain-text reply with no tool call.
			if submitted {
				// Work already handed to the integrator; a trailing plain-text
				// wrap-up is a legitimate finish.
				return AgentLoopResult{Reply: orNoReply(lastText), Usage: total, Steps: step + 1, Submitted: submitted}, nil
			}
			plainTextRetries++
			if plainTextRetries > maxPlainTextRetries {
				// Model keeps refusing tools; give up rather than burn tokens.
				return AgentLoopResult{Reply: orNoReply(lastText), Usage: total, Steps: step + 1, Submitted: submitted}, nil
			}
			messages = append(messages, loopMessage{
				Role:    "user",
				Content: "You have not called any tool yet and nothing was submitted. Plain text does not complete the task. You MUST call a tool now: use bash/write to make progress, http to claim or submit work, or finish(summary) only when the work is actually done.",
			})
			continue
		}

		// Execute tool calls; finish short-circuits. Independent calls in the same
		// round run CONCURRENTLY (Go 1.25+ emits parallel tool calls when the model
		// batches independent ops like "write 3 files"), which collapses what used
		// to be N sequential round-trips into one. Results are written back in the
		// ORIGINAL call order so the transcript stays deterministic for the model.
		finished := false
		var finishSummary string
		type toolResult struct {
			id, name, out string
		}
		results := make([]toolResult, len(msg.ToolCalls))

		// submitted is mutated by runHTTPTool; guard for the concurrent fan-out.
		var subMu sync.Mutex
		subPtr := &submitted
		setSub := func() { subMu.Lock(); *subPtr = true; subMu.Unlock() }

		var wg sync.WaitGroup
		for i, tc := range msg.ToolCalls {
			name := tc.Function.Name
			var args map[string]any
			if err := json.Unmarshal([]byte(tc.Function.Arguments), &args); err != nil {
				args = map[string]any{}
			}

			if name == "finish" {
				finished = true
				if s, ok := args["summary"].(string); ok {
					finishSummary = s
				}
				results[i] = toolResult{tc.ID, name, "turn ended"}
				continue
			}

			wg.Add(1)
			go func(i int, tcID, name string, args map[string]any) {
				defer wg.Done()
				out := d.execToolConcurrent(ctx, name, args, cfg, setSub)
				results[i] = toolResult{tcID, name, out}
			}(i, tc.ID, name, args)
		}
		wg.Wait()

		for _, r := range results {
			messages = append(messages, loopMessage{Role: "tool", ToolCallID: r.id, Name: r.name, Content: r.out})
		}

		if finished {
			reply := finishSummary
			if reply == "" {
				reply = lastText
			}
			return AgentLoopResult{Reply: orNoReply(reply), Usage: total, Steps: step + 1, Submitted: submitted}, nil
		}
	}
	return AgentLoopResult{Reply: orNoReply(lastText) + " (max steps)", Usage: total, Steps: cfg.MaxSteps, Submitted: submitted}, nil
}

func orNoReply(s string) string {
	if strings.TrimSpace(s) == "" {
		return "(no reply)"
	}
	return s
}

// roundTripLoop is like roundTrip but decodes tool_calls. Retries 429/5xx with backoff.
func (d *DirectHarness) roundTripLoop(ctx context.Context, url string, body []byte) (loopChatResponse, error) {
	var lastErr error
	for attempt := 0; attempt <= d.cfg.MaxRetries; attempt++ {
		if attempt > 0 {
			if err := d.sleep(ctx, time.Duration(attempt)*d.cfg.RetryBackoff); err != nil {
				return loopChatResponse{}, err
			}
		}
		httpReq, err := http.NewRequestWithContext(ctx, http.MethodPost, url, bytes.NewReader(body))
		if err != nil {
			return loopChatResponse{}, fmt.Errorf("harness: build request: %w", err)
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
			lastErr = fmt.Errorf("harness: transport: %w", err)
			continue
		}
		payload, err := io.ReadAll(io.LimitReader(httpResp.Body, 8<<20))
		httpResp.Body.Close()
		if err != nil {
			lastErr = fmt.Errorf("harness: read body: %w", err)
			continue
		}
		if httpResp.StatusCode < 200 || httpResp.StatusCode >= 300 {
			he := &HTTPError{StatusCode: httpResp.StatusCode, Status: httpResp.Status, Body: truncate(string(payload), 512)}
			lastErr = he
			if !he.Retryable() {
				return loopChatResponse{}, he
			}
			continue
		}
		var parsed loopChatResponse
		if err := json.Unmarshal(payload, &parsed); err != nil {
			return loopChatResponse{}, fmt.Errorf("harness: decode response: %w", err)
		}
		return parsed, nil
	}
	return loopChatResponse{}, fmt.Errorf("harness: giving up after %d attempts: %w", d.cfg.MaxRetries+1, lastErr)
}

// execTool dispatches one tool call and returns the textual result fed back to the model.
func (d *DirectHarness) execTool(ctx context.Context, name string, args map[string]any, cfg AgentLoopConfig, submitted *bool) string {
	switch name {
	case "bash":
		cmd, _ := args["cmd"].(string)
		return runBashTool(ctx, cmd, cfg.WorkDir, cfg.BashTimeout)
	case "write":
		file, _ := args["file"].(string)
		return runWriteTool(file, cfg.WorkDir)
	case "http":
		req, _ := args["request"].(string)
		return runHTTPTool(ctx, req, cfg.ServerPort, func() { *submitted = true })
	default:
		return "unknown tool: " + name
	}
}

// execToolConcurrent is the fan-out variant: takes a setSubmitted callback
// instead of a bare pointer so concurrent http tool calls can mark submission
// without a data race.
func (d *DirectHarness) execToolConcurrent(ctx context.Context, name string, args map[string]any, cfg AgentLoopConfig, setSubmitted func()) string {
	switch name {
	case "bash":
		cmd, _ := args["cmd"].(string)
		return runBashTool(ctx, cmd, cfg.WorkDir, cfg.BashTimeout)
	case "write":
		file, _ := args["file"].(string)
		return runWriteTool(file, cfg.WorkDir)
	case "http":
		req, _ := args["request"].(string)
		return runHTTPTool(ctx, req, cfg.ServerPort, setSubmitted)
	default:
		return "unknown tool: " + name
	}
}

// runBashTool executes cmd via cmd.exe in dir, bounded by timeout. Output truncated to 16k.
func runBashTool(ctx context.Context, cmd, dir string, timeout time.Duration) string {
	if strings.TrimSpace(cmd) == "" {
		return "(empty command)"
	}
	// Tool-level guard: refuse commands that edit files through a script or
	// inline interpreter. A prompt rule asking for the write tool instead is
	// advisory and the model ignored it repeatedly; this is not.
	if v := CheckBashCommand(cmd); v.Blocked {
		return "bash refused: " + v.Reason + ". " + v.Hint
	}
	cctx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	c := exec.CommandContext(cctx, "cmd", "/c", cmd)
	if dir != "" {
		c.Dir = dir
	}
	var buf bytes.Buffer
	c.Stdout = &buf
	c.Stderr = &buf
	err := c.Run()
	out := buf.String()
	if len(out) > 16000 {
		out = out[:16000] + "\n...(truncated)"
	}
	if err == nil {
		if out == "" {
			return "(no output)"
		}
		return out
	}
	timedOut := cctx.Err() == context.DeadlineExceeded
	suffix := ""
	if timedOut {
		suffix = " (TIMEOUT/KILLED)"
	}
	code := "?"
	if ee, ok := err.(*exec.ExitError); ok {
		code = fmt.Sprintf("%d", ee.ExitCode())
	}
	if out == "" {
		out = err.Error()
	}
	return fmt.Sprintf("exit %s%s:\n%s", code, suffix, out)
}

// runWriteTool writes {path,content} JSON under dir; rejects paths escaping dir.
func runWriteTool(fileJSON, dir string) string {
	if strings.TrimSpace(fileJSON) == "" {
		return "write failed: empty file payload"
	}
	var f struct {
		Path    string `json:"path"`
		Content string `json:"content"`
	}
	if err := json.Unmarshal([]byte(fileJSON), &f); err != nil {
		return "write failed: bad JSON: " + err.Error()
	}
	if f.Path == "" {
		return "write failed: missing path"
	}
	clean := filepath.Clean(f.Path)
	if strings.HasPrefix(clean, "..") || filepath.IsAbs(clean) {
		return "write failed: path escapes worktree: " + f.Path
	}
	abs := filepath.Join(dir, clean)
	if err := os.MkdirAll(filepath.Dir(abs), 0o755); err != nil {
		return "write failed: " + err.Error()
	}
	if err := os.WriteFile(abs, []byte(f.Content), 0o644); err != nil {
		return "write failed: " + err.Error()
	}
	lines := strings.Count(f.Content, "\n") + 1
	return fmt.Sprintf("wrote %s (%d bytes, %d lines)", f.Path, len(f.Content), lines)
}

// runHTTPTool calls the local swarm server. request is JSON {method,path,...fields}.
// GET fields become query params; POST fields become the JSON body.
func runHTTPTool(ctx context.Context, requestJSON string, port int, setSubmitted func()) string {
	method, path, fields := normalizeHTTPInput(requestJSON)
	if path == "" {
		return "http failed: missing path"
	}
	if !strings.HasPrefix(path, "/") {
		path = "/" + path
	}
	base := fmt.Sprintf("http://127.0.0.1:%d", port)

	var req *http.Request
	var err error
	if method == "GET" {
		q := url.Values{}
		for k, v := range fields {
			q.Set(k, fmt.Sprintf("%v", v))
		}
		u := base + path
		if len(q) > 0 {
			u += "?" + q.Encode()
		}
		req, err = http.NewRequestWithContext(ctx, http.MethodGet, u, nil)
	} else {
		body, mErr := json.Marshal(fields)
		if mErr != nil {
			return "http failed: encode body: " + mErr.Error()
		}
		req, err = http.NewRequestWithContext(ctx, http.MethodPost, base+path, bytes.NewReader(body))
		if err == nil {
			req.Header.Set("Content-Type", "application/json")
		}
	}
	if err != nil {
		return "http failed: " + err.Error()
	}

	client := &http.Client{Timeout: 30 * time.Second}
	resp, err := client.Do(req)
	if err != nil {
		return "http failed: " + err.Error()
	}
	defer resp.Body.Close()
	payload, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	text := string(payload)
	if len(text) > 8000 {
		text = text[:8000] + "...(truncated)"
	}
	out := fmt.Sprintf("%d %s", resp.StatusCode, text)
	if method == "POST" && path == "/submit" && resp.StatusCode >= 200 && resp.StatusCode < 300 {
		setSubmitted()
	}
	return out
}

// normalizeHTTPInput accepts the single-string 'request' JSON, plus legacy
// flat/nested shapes, mirroring the JS harness.
func normalizeHTTPInput(requestJSON string) (method, path string, fields map[string]any) {
	method = "GET"
	fields = map[string]any{}
	var j map[string]any
	if err := json.Unmarshal([]byte(requestJSON), &j); err != nil {
		return method, path, fields
	}
	if m, ok := j["method"].(string); ok && m != "" {
		method = strings.ToUpper(m)
	}
	if p, ok := j["path"].(string); ok {
		path = p
	}
	for k, v := range j {
		if k == "method" || k == "path" {
			continue
		}
		if v == nil {
			continue
		}
		if s, ok := v.(string); ok && s == "" {
			continue
		}
		fields[k] = v
	}
	return method, path, fields
}

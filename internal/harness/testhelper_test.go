package harness

import (
	"encoding/json"
	"net/http"
)

// decodeJSON decodes an HTTP request body into v; it is test-only scaffolding
// for asserting what the harness actually put on the wire.
func decodeJSON(r *http.Request, v any) error {
	defer r.Body.Close()
	return json.NewDecoder(r.Body).Decode(v)
}

package store

import (
	"testing"

	"swarm-go/internal/server"
)

// TestLooksLikeResourceRejectsProse is the regression guard for the
// auto-split explosion: a real run split one item into "error.messag",
// "error.msg", "error.error", "error.detail" because the old regex treated
// any dotted word as a file path.
func TestLooksLikeResourceRejectsProse(t *testing.T) {
	bad := []string{
		"error.message", "error.msg", "error.error_msg", "error.detail",
		"error.error", "v1.2.3", "e.g", "i.e",
		"foo.bar", "a.b", "no_extension", "trailing.", ".hidden",
		"星号.com", "sessionid.value", "node.version",
	}
	for _, tok := range bad {
		if looksLikeResource(tok) {
			t.Errorf("%q must NOT be treated as a resource", tok)
		}
	}

	good := []string{
		"src/util.js", "src/openai/http.js", "internal/store/items.go",
		"test/http.test.js", "./a.js", "a/b/c.d",
		"README.md", "package.json",
		"go.mod", "main.go", "x.ts", "y.mjs", "z.ps1",
		"src/util.js#compress", "a.js#fnA",
		`internal\store\items.go`,
	}
	for _, tok := range good {
		if !looksLikeResource(tok) {
			t.Errorf("%q MUST be treated as a resource", tok)
		}
	}
}

// TestItemResourcesOnRealSeedDetail reproduces the exact detail text that
// caused the explosion, and asserts only real files come out (no
// "error.messag" / "error.msg" / "error.detail" junk).
func TestItemResourcesOnRealSeedDetail(t *testing.T) {
	it := server.Item{
		Title: "src/dola/sse.js#errorMessageOf",
		Detail: `sse.js 的 errorMessageOf 目前只处理 error_msg / message 两个字段。扩展为按优先级尝试：
1) error.message（英文语义字段）
2) error.msg（本地化字段）
3) error.error_msg
4) error.detail
都取不到时回落到 'upstream error'。保留现有 [error_code] 前缀行为。
在 test/sse.test.js 里为每个优先级各补一个用例。
验证：node --test test/sse.test.js`,
	}
	got := itemResources(it)
	junk := map[string]bool{
		"error.messag": true, "error.msg": true, "error.error": true, "error.detail": true,
	}
	if len(got) == 0 {
		t.Fatal("no resources extracted at all")
	}
	for _, r := range got {
		if junk[r] {
			t.Fatalf("prose leaked into resources: %v", got)
		}
	}
	// The item's own title resource must be present.
	found := false
	for _, r := range got {
		if r == "src/dola/sse.js#errormessageof" {
			found = true
		}
	}
	if !found {
		t.Fatalf("title resource missing from %v", got)
	}
}

// TestArtifactKeyIgnoresProse: dedup must not fold on prose either.
func TestArtifactKeyIgnoresProse(t *testing.T) {
	if k := artifactKey("handle error.message and error.detail properly"); k != "" {
		t.Fatalf("prose-only title must yield no key, got %q", k)
	}
	if k := artifactKey("create src/util/compact.js#compress"); k == "" {
		t.Fatal("a real path must still yield a key")
	}
}

// TestAutoSplitWouldNotExplode: the resource list that drives autoSplitItem
// must stay bounded by the real files an item names.
func TestAutoSplitWouldNotExplode(t *testing.T) {
	res := ResourcesOf(server.Item{
		Title:  "src/dola/sse.js#errorMessageOf",
		Detail: "try error.message, then error.msg, then error.detail",
	})
	if len(res) > 3 {
		t.Fatalf("resource list exploded to %d: %v", len(res), res)
	}
}

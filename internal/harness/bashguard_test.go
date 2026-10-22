package harness

import (
	"strings"
	"testing"
)

// TestBashGuardBlocksScriptEdits covers the patterns a real run produced.
func TestBashGuardBlocksScriptEdits(t *testing.T) {
	blocked := []string{
		// the exact shapes observed on dola
		`node tmp_patch.mjs`,
		`node tmp_patch2.mjs src/openai/http.js`,
		`node scripts/_grep.js`,
		`node tmp_t68_patch.js`,
		`node tmp_addtests.js`,
		`python3 fix_util.py`,
		`py patch.py`,
		// in-place editors
		`sed -i 's/foo/bar/' src/util.js`,
		`sed -i.bak 's/a/b/' test/x.test.js`,
		`perl -pi -e 's/x/y/' src/a.js`,
		// inline interpreter writing files
		`node -e "require('fs').writeFileSync('src/a.js','x')"`,
		`node -e "const fs=require('fs');fs.appendFileSync('src/a.js','y')"`,
		`python3 -c "open('src/a.py','w').write('x')"`,
		// redirection into source
		`echo "module.exports={}" > src/new.js`,
		`echo 'x' >> src/util.js`,
		`echo hi > internal\store\items.go`,
		// chained
		`cd src && node ../tmp_patch.mjs && node --test test/`,
	}
	for _, cmd := range blocked {
		if v := CheckBashCommand(cmd); !v.Blocked {
			t.Errorf("should block: %s", cmd)
		}
	}
}

// TestBashGuardAllowsNormalWork is the more important half: a guard that blocks
// legitimate commands stops the swarm from working at all.
func TestBashGuardAllowsNormalWork(t *testing.T) {
	allowed := []string{
		// tests and builds — the whole point of bash
		`node --test test/`,
		`node --test test/http.test.js`,
		`go test ./...`,
		`go build ./...`,
		`go vet ./...`,
		`npm test`,
		`npm run build`,
		// git
		`git status`,
		`git diff --stat`,
		`git log --oneline -5`,
		`git add -A`,
		// reads
		`cat src/util.js`,
		`type src\util.js`,
		`head -50 test/http.test.js`,
		`grep -n "readJson" src/openai/http.js`,
		`ls -la`,
		`dir`,
		// running the project itself (not a patch script)
		`node src/server.js`,
		`node --version`,
		// scratch output — how workers inspect things
		`node --test test/ > out.txt`,
		`node --test test/ > tmp_out.txt 2>&1`,
		`echo "--- result ---"`,
		`node -e "console.log(1+1)"`,
		`node -e "const fs=require('fs');console.log(fs.readFileSync('src/a.js','utf8').length)"`,
		// node_modules and vendored deps are normal to execute
		`node_modules\.bin\eslint src/`,
		`npx tsc --noEmit`,
		// empty-ish
		``,
		`   `,
	}
	for _, cmd := range allowed {
		if v := CheckBashCommand(cmd); v.Blocked {
			t.Errorf("should ALLOW %q (reason: %s)", cmd, v.Reason)
		}
	}
}

// TestBashGuardRedirectionEdgeCases: the redirect rule must not become a
// blanket ban on output capture.
func TestBashGuardRedirectionEdgeCases(t *testing.T) {
	// /dev/null and log/txt sinks are fine
	for _, ok := range []string{
		`node --test test/ > /dev/null`,
		`node --test test/ > /dev/null 2>&1`,
		`go build ./... > build.log`,
		`node --test test/ >> run.log`,
		`node --test test/ > out.txt`,
	} {
		if v := CheckBashCommand(ok); v.Blocked {
			t.Errorf("should ALLOW redirect %q (reason: %s)", ok, v.Reason)
		}
	}
	// source sinks are not
	for _, bad := range []string{
		`node --test test/ > src/x.js`,
		`cat a.txt > src/util.js`,
	} {
		if v := CheckBashCommand(bad); !v.Blocked {
			t.Errorf("should BLOCK redirect %q", bad)
		}
	}
}

// TestBashGuardMessageIsActionable: the refusal must tell the model what to do
// instead, or it just retries the same thing.
func TestBashGuardMessageIsActionable(t *testing.T) {
	v := CheckBashCommand(`node tmp_patch.mjs`)
	if !v.Blocked {
		t.Fatal("expected blocked")
	}
	if !strings.Contains(v.Hint, "write tool") {
		t.Errorf("hint should point at the write tool, got %q", v.Hint)
	}
	if v.Reason == "" {
		t.Error("reason must name what was refused")
	}
}

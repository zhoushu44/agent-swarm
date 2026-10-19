package gitutil

import (
	"os"
	"path/filepath"
)

func runtimeIsWindows() bool {
	return os.PathSeparator == '\\'
}

// IntegrationPath returns the default on-disk path of the integration worktree.
func (r *Repo) IntegrationPath() string {
	return filepath.Join(r.Dir, ".swarm", "integration")
}

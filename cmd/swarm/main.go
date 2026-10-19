// Command swarm is the CLI entry point for the swarm distributed task runner.
//
// Usage:
//
//	swarm init <dir>           Initialize a new swarm workspace
//	swarm seed <dir> --file f  Seed work items from JSON file
//	swarm run <dir> [flags]    Run the swarm (autoscale workers, integrate results)
//	swarm board <dir>          Show board status
//	swarm status <dir>         Show detailed status
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"

	"swarm-go/internal/server"
	"swarm-go/internal/store"
)

const swarmDir = ".swarm"

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintf(os.Stderr, "usage: swarm <command> [args]\n")
		fmt.Fprintf(os.Stderr, "commands: init, seed, run, board, status\n")
		os.Exit(1)
	}
	cmd := os.Args[1]
	args := os.Args[2:]

	switch cmd {
	case "init":
		if len(args) < 1 {
			fmt.Fprintf(os.Stderr, "usage: swarm init <dir>\n")
			os.Exit(1)
		}
		if err := cmdInit(args[0]); err != nil {
			fmt.Fprintf(os.Stderr, "init: %v\n", err)
			os.Exit(1)
		}
		fmt.Printf("initialized %s\n", args[0])

	case "seed":
		if len(args) < 1 {
			fmt.Fprintf(os.Stderr, "usage: swarm seed <dir> --file <seed.json>\n")
			os.Exit(1)
		}
		dir := args[0]
		var file string
		for i := 1; i < len(args); i++ {
			if args[i] == "--file" && i+1 < len(args) {
				file = args[i+1]
				i++
			}
		}
		if file == "" {
			fmt.Fprintf(os.Stderr, "seed: --file required\n")
			os.Exit(1)
		}
		if err := cmdSeed(dir, file); err != nil {
			fmt.Fprintf(os.Stderr, "seed: %v\n", err)
			os.Exit(1)
		}
		fmt.Printf("seeded %s from %s\n", dir, file)

	case "run":
		if len(args) < 1 {
			fmt.Fprintf(os.Stderr, "usage: swarm run <dir> [flags]\n")
			os.Exit(1)
		}
		dir := args[0]
		cfg := defaultRunConfig()
		for i := 1; i < len(args); i++ {
			sw := func(dst *string) {
				if i+1 < len(args) {
					*dst = args[i+1]
					i++
				}
			}
			iw := func(dst *int) {
				if i+1 < len(args) {
					fmt.Sscanf(args[i+1], "%d", dst)
					i++
				}
			}
			i64 := func(dst *int64) {
				if i+1 < len(args) {
					fmt.Sscanf(args[i+1], "%d", dst)
					i++
				}
			}
			switch args[i] {
			case "--max-workers":
				iw(&cfg.maxWorkers)
			case "--min-workers":
				iw(&cfg.minWorkers)
			case "--workers":
				iw(&cfg.initialWorkers)
			case "--max-turns":
				iw(&cfg.maxTurns)
			case "--lease-sec":
				iw(&cfg.leaseSec)
			case "--max-items":
				iw(&cfg.maxItems)
			case "--max-item-turns":
				iw(&cfg.maxItemTurns)
			case "--max-item-tokens":
				iw(&cfg.maxItemTokens)
			case "--max-item-fails":
				iw(&cfg.maxItemFails)
			case "--budget-tokens":
				i64(&cfg.budgetTokens)
			case "--server-port":
				iw(&cfg.serverPort)
			case "--harness":
				sw(&cfg.harnessName)
			case "--api-base":
				sw(&cfg.apiBase)
			case "--api-key":
				sw(&cfg.apiKey)
			case "--model":
				sw(&cfg.model)
			case "--mission":
				sw(&cfg.mission)
			case "--conventions":
				sw(&cfg.conventions)
			case "--test-cmd":
				sw(&cfg.testCmd)
			}
		}
		if err := cmdRun(dir, cfg); err != nil {
			fmt.Fprintf(os.Stderr, "run: %v\n", err)
			os.Exit(1)
		}

	case "board":
		if len(args) < 1 {
			fmt.Fprintf(os.Stderr, "usage: swarm board <dir>\n")
			os.Exit(1)
		}
		if err := cmdBoard(args[0]); err != nil {
			fmt.Fprintf(os.Stderr, "board: %v\n", err)
			os.Exit(1)
		}

	case "status":
		if len(args) < 1 {
			fmt.Fprintf(os.Stderr, "usage: swarm status <dir>\n")
			os.Exit(1)
		}
		if err := cmdStatus(args[0]); err != nil {
			fmt.Fprintf(os.Stderr, "status: %v\n", err)
			os.Exit(1)
		}

	default:
		fmt.Fprintf(os.Stderr, "unknown command %q\n", cmd)
		fmt.Fprintf(os.Stderr, "usage: swarm <command> [args]\n")
		fmt.Fprintf(os.Stderr, "commands: init, seed, run, board, status\n")
		os.Exit(1)
	}
}

func dbPath(dir string) string {
	return filepath.Join(dir, swarmDir, "swarm.db")
}

func openStore(dir string) (*store.Store, error) {
	p := dbPath(dir)
	if err := os.MkdirAll(filepath.Dir(p), 0o755); err != nil {
		return nil, err
	}
	return store.Open(p)
}

func cmdInit(dir string) error {
	if err := os.MkdirAll(filepath.Join(dir, swarmDir), 0o755); err != nil {
		return err
	}
	st, err := openStore(dir)
	if err != nil {
		return err
	}
	if err := st.Close(); err != nil {
		return err
	}
	// create a git repository so worktree/merge operations work
	cmd := exec.Command("git", "init")
	cmd.Dir = dir
	return cmd.Run()
}

func cmdSeed(dir, file string) error {
	st, err := openStore(dir)
	if err != nil {
		return err
	}
	defer st.Close()

	data, err := os.ReadFile(file)
	if err != nil {
		return err
	}
	var items []struct {
		Title    string `json:"title"`
		Detail   string `json:"detail"`
		ParentID string `json:"parentId,omitempty"`
		Kind     string `json:"kind,omitempty"`
		Priority int    `json:"priority,omitempty"`
	}
	if err := json.Unmarshal(data, &items); err != nil {
		return err
	}
	for _, it := range items {
		var parentID *string
		if it.ParentID != "" {
			parentID = &it.ParentID
		}
		_, err := st.CreateItem(server.CreateItemInput{
			Peer:     "seed",
			Title:    it.Title,
			Detail:   it.Detail,
			ParentID: parentID,
			Kind:     it.Kind,
			Priority: it.Priority,
		})
		if err != nil {
			return err
		}
	}
	return nil
}

func cmdRun(dir string, cfg runConfig) error {
	st, err := openStore(dir)
	if err != nil {
		return err
	}
	defer st.Close()

	r, err := newSwarmRunner(dir, cfg, st)
	if err != nil {
		return err
	}
	fmt.Printf("swarm run: dir=%s workers=%d maxWorkers=%d port=%d harness=%s\n",
		dir, cfg.initialWorkers, cfg.maxWorkers, cfg.serverPort, cfg.harnessName)
	return r.run(context.Background())
}

func cmdBoard(dir string) error {
	st, err := openStore(dir)
	if err != nil {
		return err
	}
	defer st.Close()

	items, total, err := st.ListItems("", 100, 0)
	if err != nil {
		return err
	}
	fmt.Printf("board: %d items\n", total)
	for _, it := range items {
		fmt.Printf("  [%s] %s\n", it.Status, it.Title)
	}
	return nil
}

func cmdStatus(dir string) error {
	st, err := openStore(dir)
	if err != nil {
		return err
	}
	defer st.Close()

	digest, err := st.Digest("cli", 0)
	if err != nil {
		return err
	}
	fmt.Printf("status: %d total, %d pending, %d in_progress, %d done, %d failed\n",
		digest.Stats.Total,
		digest.Stats.Pending,
		digest.Stats.InProgress,
		digest.Stats.Done,
		digest.Stats.Failed,
	)
	return nil
}

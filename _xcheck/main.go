package main

import (
	"os"

	"swarm-go/internal/prompts"
)

func main() {
	harness := "direct"
	if len(os.Args) > 1 {
		harness = os.Args[1]
	}
	out := prompts.BuildWorkerPrompt(prompts.Options{
		PeerName:    "w1",
		ServerPort:  17771,
		Token:       "tok-abc",
		Mission:     "port the swarm runtime to Go",
		Conventions: "go 1.22, no cgo",
		Turn:        3,
		Brief:       "BOARD SNAPSHOT: 1 pending",
		Harness:     harness,
	})
	os.Stdout.WriteString(out)
}

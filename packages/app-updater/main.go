// passession-update-helper is a short-lived desktop-owned updater, not a service.
package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"runtime"
	"strings"
	"time"
)

type processIdentity struct {
	PID        int    `json:"pid"`
	BirthToken string `json:"birthToken"`
}

var uuidPattern = regexp.MustCompile(`^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$`)
var digestPattern = regexp.MustCompile(`^[0-9a-f]{128}$`)

func main() {
	if len(os.Args) == 2 && os.Args[1] == "--identity" {
		if err := json.NewEncoder(os.Stdout).Encode(helperIdentity()); err != nil {
			os.Exit(1)
		}
		return
	}
	if len(os.Args) != 3 || os.Args[1] != "--request" {
		os.Exit(2)
	}
	if err := runComponentRequest(os.Args[2]); err != nil {
		fmt.Fprintln(os.Stderr, "Desktop update helper:", err)
		os.Exit(1)
	}
}

func terminal(phase string) bool {
	return phase == "committed" || phase == "failed" || phase == "rolledBack"
}
func waitUntil(timeout, interval time.Duration, predicate func() bool) bool {
	deadline := time.Now().Add(timeout)
	for {
		if predicate() {
			return true
		}
		if time.Now().After(deadline) {
			return false
		}
		time.Sleep(interval)
	}
}
func normalizedWindowsPath(path string) string {
	path = filepath.Clean(path)
	if strings.HasPrefix(strings.ToUpper(path), `\\?\UNC\`) {
		return `\\` + path[8:]
	}
	if strings.HasPrefix(path, `\\?\`) {
		return path[4:]
	}
	return path
}
func samePath(a, b string) bool {
	if runtime.GOOS == "windows" {
		return strings.EqualFold(normalizedWindowsPath(a), normalizedWindowsPath(b))
	}
	return a == b
}
func commandOutput(path string, args ...string) (string, error) {
	cmd := exec.Command(path, args...)
	cmd.Env = append(os.Environ(), "LC_ALL=C")
	hideWindow(cmd)
	var out bytes.Buffer
	cmd.Stdout = &out
	if err := cmd.Run(); err != nil {
		return "", err
	}
	if out.Len() > maxJSON {
		return "", errors.New("native metadata exceeds limit")
	}
	return strings.TrimSpace(out.String()), nil
}

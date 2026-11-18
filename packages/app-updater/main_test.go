package main

import (
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"slices"
	"testing"
)

func exeSuffix() string {
	if runtime.GOOS == "windows" {
		return ".exe"
	}
	return ""
}
func mustWrite(t *testing.T, path string, data []byte, mode os.FileMode) {
	t.Helper()
	if err := os.WriteFile(path, data, mode); err != nil {
		t.Fatal(err)
	}
}
func mustMkdir(t *testing.T, path string) {
	t.Helper()
	if err := os.MkdirAll(path, 0700); err != nil {
		t.Fatal(err)
	}
}
func fixtureBinary(t *testing.T, path string) {
	t.Helper()
	exe, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	exe, err = filepath.EvalSymlinks(exe)
	if err != nil {
		t.Fatal(err)
	}
	if err = copyFile(exe, path, 0755); err != nil {
		t.Fatal(err)
	}
}
func canonicalTemp(t *testing.T) string {
	t.Helper()
	path, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	return path
}
func fixtureProcess(t *testing.T, path, role string) (processIdentity, *exec.Cmd) {
	t.Helper()
	cmd := exec.Command(path)
	cmd.Env = append(os.Environ(), "PASEO_UPDATER_FIXTURE_ROLE="+role)
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	p, err := inspectProcess(cmd.Process.Pid)
	if err != nil {
		_ = cmd.Process.Kill()
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if !exited(p.identity) {
			_ = cmd.Process.Kill()
		}
		_ = cmd.Wait()
	})
	return p.identity, cmd
}
func killFixture(p processIdentity) {
	if p.PID <= 1 || exited(p) {
		return
	}
	current, err := inspectProcess(p.PID)
	if err != nil || current.identity != p {
		return
	}
	child, err := os.FindProcess(p.PID)
	if err == nil {
		_ = child.Kill()
	}
}
func TestProcessBirthIdentityCannotBeReused(t *testing.T) {
	me, err := inspectProcess(os.Getpid())
	if err != nil {
		t.Fatal(err)
	}
	if exited(me.identity) {
		t.Fatal("current process reported dead")
	}
	changed := me.identity
	changed.BirthToken += "-different"
	if !exited(changed) {
		t.Fatal("reused PID accepted as original parent")
	}
}
func TestExclusiveLeaseAndRelease(t *testing.T) {
	path := filepath.Join(canonicalTemp(t), "lease")
	first, err := acquireLease(path)
	if err != nil {
		t.Fatal(err)
	}
	if second, err := acquireLease(path); err == nil {
		second.Close()
		first.Close()
		t.Fatal("duplicate lease accepted")
	}
	first.Close()
	next, err := acquireLease(path)
	if err != nil {
		t.Fatal(err)
	}
	next.Close()
}
func TestDigestAndSymlinkSafety(t *testing.T) {
	root := canonicalTemp(t)
	file := filepath.Join(root, "artifact")
	mustWrite(t, file, []byte("one"), 0600)
	before, err := digest(file)
	if err != nil {
		t.Fatal(err)
	}
	mustWrite(t, file, []byte("two"), 0600)
	after, err := digest(file)
	if err != nil {
		t.Fatal(err)
	}
	if before == after {
		t.Fatal("digest did not bind bytes")
	}
	link := filepath.Join(root, "link")
	if err = os.Symlink(file, link); err != nil {
		if runtime.GOOS == "windows" {
			t.Skip("symlink privilege unavailable")
		}
		t.Fatal(err)
	}
	if _, err = digest(link); err == nil {
		t.Fatal("symlink artifact accepted")
	}
}
func TestRelaunchPreservesHomeAndClearsStaleUpdateState(t *testing.T) {
	o := componentOperation{
		request: componentRequest{
			OperationDirectory: "/isolated/op",
			OperationID:        "operation",
			StoreDirectory:     "/isolated/store",
			BaseRoot:           "/isolated/base",
		},
		journal: componentJournal{Status: componentStatus{Nonce: "fresh"}},
	}
	for _, home := range [][]string{nil, {"PASEO_HOME=/independent/home"}, {"PASEO_HOME="}} {
		base := append(slices.Clone(home), "KEEP=present", "PASEO_UPDATE_NONCE=stale", "PASEO_UPDATE_OTHER=stale",
			"PASEO_COMPONENT_STORE=/old", "PASEO_COMPONENT_BASE_ROOT=/old", "APPIMAGE=/old", "APPDIR=/old-mount",
			"APPIMAGE_EXTRACT_AND_RUN=stale", "LD_LIBRARY_PATH=/old-libs", "LD_PRELOAD=/old-lib", "LD_AUDIT=/old-audit")
		want := append(slices.Clone(home), "KEEP=present", "PASEO_UPDATE_OPERATION_DIRECTORY=/isolated/op",
			"PASEO_UPDATE_OPERATION_ID=operation", "PASEO_UPDATE_NONCE=fresh", "PASEO_COMPONENT_STORE=/isolated/store",
			"PASEO_COMPONENT_BASE_ROOT=/isolated/base")
		if env := o.launchEnvironment(base); !slices.Equal(env, want) {
			t.Fatalf("launch environment = %q, want %q", env, want)
		}
	}
}

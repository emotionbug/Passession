package main

import (
	"archive/zip"
	"bytes"
	"errors"
	"os"
	"path/filepath"
	"testing"
)

func TestWindowsUpdaterOwnsNewOutputs(t *testing.T) {
	// Model the GUI's private directory with a real inheritable Windows DACL.
	// In an elevated process this owner differs from the token's default owner.
	sid, err := currentSID()
	if err != nil {
		t.Fatal(err)
	}
	descriptor, err := securityDescriptor("O:" + sid + "D:P(A;OICI;FA;;;" + sid + ")")
	if err != nil {
		t.Fatal(err)
	}
	root := filepath.Join(canonicalTemp(t), "private")
	if err = mkdirWithDescriptor(root, descriptor); err != nil {
		t.Fatal(err)
	}
	checkPrivate := func(path string, directory bool) {
		t.Helper()
		if err := privatePath(path, directory); err != nil {
			t.Fatalf("%s: %v", path, err)
		}
	}
	checkPrivate(root, true)

	status := filepath.Join(root, "status.json")
	for _, phase := range []string{"preparation", "waitingForParent"} {
		if err = atomicJSON(status, map[string]string{"phase": phase}); err != nil {
			t.Fatal(err)
		}
		checkPrivate(status, false)
	}
	leasePath := filepath.Join(root, "lease")
	for range 2 {
		lease, err := acquireLease(leasePath)
		if err != nil {
			t.Fatal(err)
		}
		if err = lease.Close(); err != nil {
			t.Fatal(err)
		}
		checkPrivate(leasePath, false)
	}

	// With no explicit directory entries, extraction must own the implicit
	// directories as well as the staging root and payload.
	source := filepath.Join(root, "source.zip")
	f, err := os.Create(source)
	if err != nil {
		t.Fatal(err)
	}
	z := zip.NewWriter(f)
	entry, err := z.Create("bin/sub/payload")
	if err != nil {
		t.Fatal(err)
	}
	if _, err = entry.Write([]byte("payload")); err != nil {
		t.Fatal(err)
	}
	if err = z.Close(); err != nil {
		t.Fatal(err)
	}
	if err = f.Close(); err != nil {
		t.Fatal(err)
	}
	retained := filepath.Join(root, "retained.zip")
	if err = copyFile(source, retained, 0400); err != nil {
		t.Fatal(err)
	}
	checkPrivate(retained, false)
	stage := filepath.Join(root, ".stage")
	if err = extractComponent(retained, "zip", stage); err != nil {
		t.Fatal(err)
	}
	checkPrivate(stage, true)
	checkPrivate(filepath.Join(stage, "bin"), true)
	checkPrivate(filepath.Join(stage, "bin", "sub"), true)
	checkPrivate(filepath.Join(stage, "bin", "sub", "payload"), false)

	preexisting := filepath.Join(root, "preexisting")
	mustWrite(t, preexisting, []byte("keep"), 0600)
	before := ownedPath(preexisting)
	if err := copyFile(retained, preexisting, 0600); !errors.Is(err, os.ErrExist) {
		t.Fatalf("exclusive copy over existing file: %v", err)
	}
	if after := ownedPath(preexisting); (before == nil) != (after == nil) {
		t.Fatalf("existing file ownership changed: before %v, after %v", before, after)
	}
	if contents, err := os.ReadFile(preexisting); err != nil || string(contents) != "keep" {
		t.Fatalf("existing contents = %q, %v", contents, err)
	}
}

func TestWindowsUpdaterRejectsComponentLinks(t *testing.T) {
	var contents bytes.Buffer
	z := zip.NewWriter(&contents)
	payload, err := z.Create("payload")
	if err != nil {
		t.Fatal(err)
	}
	if _, err = payload.Write([]byte("keep")); err != nil {
		t.Fatal(err)
	}
	header := &zip.FileHeader{Name: "alias"}
	header.SetMode(os.ModeSymlink | 0700)
	link, err := z.CreateHeader(header)
	if err != nil {
		t.Fatal(err)
	}
	if _, err = link.Write([]byte("payload")); err != nil {
		t.Fatal(err)
	}
	if err = z.Close(); err != nil {
		t.Fatal(err)
	}
	dir := canonicalTemp(t)
	archive := filepath.Join(dir, "component.zip")
	mustWrite(t, archive, contents.Bytes(), 0600)
	stage := filepath.Join(dir, ".stage")
	if err = extractComponent(archive, "zip", stage); err == nil || err.Error() != "Windows component symlinks are not supported" {
		t.Fatalf("extracting symlink: %v", err)
	}
	if _, err = os.Lstat(filepath.Join(stage, "alias")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("unexpected alias: %v", err)
	}
	if actual, err := os.ReadFile(filepath.Join(stage, "payload")); err != nil || string(actual) != "keep" {
		t.Fatalf("target contents = %q, %v", actual, err)
	}
}

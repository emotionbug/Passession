package main

import (
	"bytes"
	"crypto/sha512"
	"encoding/hex"
	"encoding/json"
	"errors"
	"io"
	"os"
	"path/filepath"
	"strings"
)

const maxJSON = 64 * 1024

func readJSON(path string, value any) ([]byte, error) {
	if err := regular(path); err != nil {
		return nil, err
	}
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	b, err := io.ReadAll(io.LimitReader(f, maxJSON+1))
	if err != nil {
		return nil, err
	}
	if len(b) > maxJSON {
		return nil, errors.New("JSON exceeds size limit")
	}
	if err = unambiguousJSON(b); err != nil {
		return nil, err
	}
	d := json.NewDecoder(bytes.NewReader(b))
	d.DisallowUnknownFields()
	if err = d.Decode(value); err != nil {
		return nil, err
	}
	if err = d.Decode(new(any)); err != io.EOF {
		return nil, errors.New("trailing JSON content")
	}
	return b, nil
}

func atomicJSON(path string, value any) error {
	b, err := json.Marshal(value)
	if err != nil {
		return err
	}
	if len(b) > maxJSON {
		return errors.New("JSON exceeds size limit")
	}
	f, err := createOwnedTemp(filepath.Dir(path))
	if err != nil {
		return err
	}
	temp := f.Name()
	defer os.Remove(temp)
	if err = f.Chmod(0600); err == nil {
		_, err = f.Write(append(b, '\n'))
	}
	if err == nil {
		err = f.Sync()
	}
	closeErr := f.Close()
	if err == nil {
		err = closeErr
	}
	if err != nil {
		return err
	}
	if err = replaceFile(temp, path); err != nil {
		return err
	}
	return syncDirectory(filepath.Dir(path))
}

func within(root, path string) bool {
	r, err := filepath.Rel(root, path)
	return err == nil && r != "." && r != ".." && !strings.HasPrefix(r, ".."+string(filepath.Separator)) && !filepath.IsAbs(r)
}

// Every existing path component must be real. In particular, never follow a
// request-supplied symlink into user data while copying or replacing an app.
func cleanPath(path string) error {
	if !filepath.IsAbs(path) || filepath.Clean(path) != path || strings.ContainsAny(path, "\x00\r\n") {
		return errors.New("path must be normalized and absolute")
	}
	for p := path; ; p = filepath.Dir(p) {
		st, err := os.Lstat(p)
		if err != nil {
			return err
		}
		if st.Mode()&os.ModeSymlink != 0 {
			return errors.New("path contains a symbolic link")
		}
		if p == filepath.Dir(p) {
			break
		}
	}
	return nil
}

// removeTree deletes an updater-owned tree without following a symlinked path
// component; an absent tree is already removed.
func removeTree(path string) error {
	if _, err := os.Lstat(path); errors.Is(err, os.ErrNotExist) {
		return nil
	}
	if err := cleanPath(path); err != nil {
		return err
	}
	return os.RemoveAll(path)
}
func regular(path string) error {
	if err := cleanPath(path); err != nil {
		return err
	}
	st, err := os.Lstat(path)
	if err != nil {
		return err
	}
	if !st.Mode().IsRegular() {
		return errors.New("expected a regular file")
	}
	return nil
}
func digest(path string) (string, error) {
	if err := regular(path); err != nil {
		return "", err
	}
	f, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer f.Close()
	h := sha512.New()
	if _, err = io.Copy(h, f); err != nil {
		return "", err
	}
	return hex.EncodeToString(h.Sum(nil)), nil
}
func copyFile(source, destination string, mode os.FileMode) error {
	if err := regular(source); err != nil {
		return err
	}
	in, err := os.Open(source)
	if err != nil {
		return err
	}
	defer in.Close()
	out, err := createOwnedFile(destination, os.O_WRONLY, mode)
	if err != nil {
		return err
	}
	_, err = io.Copy(out, in)
	if err == nil {
		err = out.Sync()
	}
	ce := out.Close()
	if err == nil {
		err = ce
	}
	return err
}

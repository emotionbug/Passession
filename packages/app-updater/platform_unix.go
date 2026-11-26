//go:build linux || darwin

package main

import (
	"errors"
	"os"
	"os/exec"
	"syscall"
)

type processInfo struct {
	identity   processIdentity
	executable string
	uid        uint32
}

func ownedProcess(p processInfo) error {
	if p.uid != uint32(os.Geteuid()) {
		return errors.New("process belongs to another user")
	}
	return nil
}
func ownedPath(path string) error {
	st, err := os.Lstat(path)
	if err != nil {
		return err
	}
	stat, ok := st.Sys().(*syscall.Stat_t)
	if !ok || stat.Uid != uint32(os.Geteuid()) {
		return errors.New("path belongs to another user")
	}
	return nil
}
func privatePath(path string, directory bool) error {
	if err := cleanPath(path); err != nil {
		return err
	}
	if err := ownedPath(path); err != nil {
		return err
	}
	st, err := os.Lstat(path)
	if err != nil {
		return err
	}
	if st.IsDir() != directory || (!directory && !st.Mode().IsRegular()) || st.Mode().Perm()&0077 != 0 {
		return errors.New("operation path must be private to its owner")
	}
	return nil
}
func createOwnedFile(path string, access int, mode os.FileMode) (*os.File, error) {
	return os.OpenFile(path, access|os.O_CREATE|os.O_EXCL, mode)
}
func createOwnedTemp(directory string) (*os.File, error) { return os.CreateTemp(directory, ".write-") }
func mkdirOwned(path string) error                       { return os.Mkdir(path, 0700) }
func mkdirAllOwned(path string) error                    { return os.MkdirAll(path, 0700) }

func acquireLease(path string) (*os.File, error) {
	if _, err := os.Lstat(path); err == nil {
		if err = privatePath(path, false); err != nil {
			return nil, err
		}
	} else if !errors.Is(err, os.ErrNotExist) {
		return nil, err
	}
	f, err := os.OpenFile(path, os.O_CREATE|os.O_RDWR|syscall.O_NOFOLLOW, 0600)
	if err != nil {
		return nil, err
	}
	if err = syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		f.Close()
		return nil, errors.New("another updater holds the update lease")
	}
	return f, nil
}
func syncDirectory(path string) error {
	f, err := os.Open(path)
	if err != nil {
		return err
	}
	defer f.Close()
	return f.Sync()
}
func replaceFile(source, destination string) error { return os.Rename(source, destination) }
func detach(cmd *exec.Cmd)                         { cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true} }
func hideWindow(cmd *exec.Cmd)                     {}
func exited(p processIdentity) bool {
	if p.PID <= 1 || p.BirthToken == "" {
		return false
	}
	current, err := inspectProcess(p.PID)
	// Permission/parse failures are not proof of death. ESRCH and ENOENT are.
	if err != nil {
		return errors.Is(err, os.ErrNotExist) || errors.Is(err, syscall.ESRCH)
	}
	return current.identity != p
}

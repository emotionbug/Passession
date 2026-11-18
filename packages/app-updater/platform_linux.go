package main

import (
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
)

func inspectProcess(pid int) (processInfo, error) {
	base := fmt.Sprintf("/proc/%d", pid)
	raw, err := os.ReadFile(filepath.Join(base, "stat"))
	if err != nil {
		return processInfo{}, err
	}
	end := strings.LastIndexByte(string(raw), ')')
	if end < 0 {
		return processInfo{}, errors.New("invalid process stat")
	}
	fields := strings.Fields(string(raw[end+1:]))
	if len(fields) < 20 {
		return processInfo{}, errors.New("incomplete process stat")
	}
	if fields[0] == "Z" || fields[0] == "X" {
		return processInfo{}, syscall.ESRCH
	}
	if _, err = strconv.ParseUint(fields[19], 10, 64); err != nil {
		return processInfo{}, err
	}
	exe, err := os.Readlink(filepath.Join(base, "exe"))
	if err != nil {
		return processInfo{}, err
	}
	st, err := os.Stat(base)
	if err != nil {
		return processInfo{}, err
	}
	return processInfo{processIdentity{pid, fields[19]}, exe, st.Sys().(*syscall.Stat_t).Uid}, nil
}

func validateAppImageProcess(p processInfo, image string) error {
	// APPIMAGE/APPDIR come from the bound local GUI process, never from request
	// paths. Do not expose the remainder of its environment in logs or status.
	f, err := os.Open(fmt.Sprintf("/proc/%d/environ", p.identity.PID))
	if err != nil {
		return err
	}
	defer f.Close()
	data, err := io.ReadAll(io.LimitReader(f, (1<<20)+1))
	if err != nil {
		return err
	}
	if len(data) > 1<<20 {
		return errors.New("process environment exceeds limit")
	}
	var actual, mount string
	for _, entry := range strings.Split(string(data), "\x00") {
		key, value, ok := strings.Cut(entry, "=")
		if !ok {
			continue
		}
		if key == "APPIMAGE" {
			actual = value
		}
		if key == "APPDIR" {
			mount = value
		}
	}
	if actual == "" || mount == "" {
		return errors.New("GUI does not identify its AppImage")
	}
	actual, err = filepath.EvalSymlinks(actual)
	if err != nil {
		return err
	}
	mount, err = filepath.EvalSymlinks(mount)
	if err != nil {
		return err
	}
	if !samePath(actual, image) || !within(mount, p.executable) {
		return errors.New("AppImage destination does not match the owning GUI")
	}
	after, err := inspectProcess(p.identity.PID)
	if err != nil {
		return err
	}
	if after.identity != p.identity {
		return errors.New("AppImage process changed during validation")
	}
	return nil
}

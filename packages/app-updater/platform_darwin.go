package main

import (
	"errors"
	"strconv"
	"strings"
	"syscall"
	"unsafe"
)

func inspectProcess(pid int) (processInfo, error) {
	if err := syscall.Kill(pid, 0); err != nil {
		return processInfo{}, err
	}
	id := strconv.Itoa(pid)
	birth, err := commandOutput("/bin/ps", "-p", id, "-o", "lstart=")
	if err != nil {
		return processInfo{}, err
	}
	uidText, err := commandOutput("/bin/ps", "-p", id, "-o", "uid=")
	if err != nil {
		return processInfo{}, err
	}
	uid, err := strconv.ParseUint(uidText, 10, 32)
	if err != nil {
		return processInfo{}, err
	}
	// proc_info(PROC_INFO_CALL_PIDINFO, pid, PROC_PIDPATHINFO, 0, buf, size).
	// Reading the kernel path avoids ps truncation and argv spoofing.
	buffer := make([]byte, 4096)
	n, _, errno := syscall.Syscall6(336, 2, uintptr(pid), 11, 0, uintptr(unsafe.Pointer(&buffer[0])), uintptr(len(buffer)))
	if errno != 0 {
		return processInfo{}, errno
	}
	if n == 0 {
		return processInfo{}, errors.New("kernel process path unavailable")
	}
	executable := strings.TrimRight(string(buffer[:n]), "\x00")
	after, err := commandOutput("/bin/ps", "-p", id, "-o", "lstart=")
	if err != nil {
		return processInfo{}, err
	}
	if birth == "" || birth != after {
		return processInfo{}, errors.New("process changed during inspection")
	}
	return processInfo{processIdentity{pid, birth}, executable, uint32(uid)}, nil
}
func validateAppImageProcess(p processInfo, image string) error {
	return errors.New("AppImage requires Linux")
}

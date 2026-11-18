package main

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"strconv"
	"syscall"
	"unsafe"
)

var kernel32 = syscall.NewLazyDLL("kernel32.dll")
var advapi32 = syscall.NewLazyDLL("advapi32.dll")
var queryImage = kernel32.NewProc("QueryFullProcessImageNameW")
var getTimes = kernel32.NewProc("GetProcessTimes")
var lockFile = kernel32.NewProc("LockFileEx")
var moveFile = kernel32.NewProc("MoveFileExW")
var securityInfo = advapi32.NewProc("GetNamedSecurityInfoW")
var getAce = advapi32.NewProc("GetAce")

type processInfo struct {
	identity   processIdentity
	executable string
	sid        string
}

func tokenSID(handle syscall.Handle) (string, error) {
	var token syscall.Token
	if err := syscall.OpenProcessToken(handle, syscall.TOKEN_QUERY, &token); err != nil {
		return "", err
	}
	defer token.Close()
	user, err := token.GetTokenUser()
	if err != nil {
		return "", err
	}
	return user.User.Sid.String()
}
func currentSID() (string, error) {
	h, err := syscall.GetCurrentProcess()
	if err != nil {
		return "", err
	}
	return tokenSID(h)
}
func inspectProcess(pid int) (processInfo, error) {
	h, err := syscall.OpenProcess(0x1000, false, uint32(pid))
	if err != nil {
		return processInfo{}, err
	}
	defer syscall.CloseHandle(h)
	var created, exit, kernel, user syscall.Filetime
	ok, _, e := getTimes.Call(uintptr(h), uintptr(unsafe.Pointer(&created)), uintptr(unsafe.Pointer(&exit)), uintptr(unsafe.Pointer(&kernel)), uintptr(unsafe.Pointer(&user)))
	if ok == 0 {
		return processInfo{}, e
	}
	if exit.HighDateTime != 0 || exit.LowDateTime != 0 {
		return processInfo{}, syscall.Errno(87)
	}
	buffer := make([]uint16, 32768)
	size := uint32(len(buffer))
	ok, _, e = queryImage.Call(uintptr(h), 0, uintptr(unsafe.Pointer(&buffer[0])), uintptr(unsafe.Pointer(&size)))
	if ok == 0 {
		return processInfo{}, e
	}
	sid, err := tokenSID(h)
	if err != nil {
		return processInfo{}, err
	}
	birth := uint64(created.HighDateTime)<<32 | uint64(created.LowDateTime)
	return processInfo{processIdentity{pid, strconv.FormatUint(birth, 10)}, syscall.UTF16ToString(buffer[:size]), sid}, nil
}
func ownedProcess(p processInfo) error {
	sid, err := currentSID()
	if err != nil {
		return err
	}
	if p.sid != sid {
		return errors.New("process belongs to another user")
	}
	return nil
}
func exited(p processIdentity) bool {
	if p.PID <= 1 || p.BirthToken == "" {
		return false
	}
	now, err := inspectProcess(p.PID)
	if err != nil {
		return errors.Is(err, syscall.Errno(87))
	}
	return now.identity != p
}

type aclHeader struct {
	Revision  uint8
	Reserved  uint8
	Size      uint16
	Count     uint16
	Reserved2 uint16
}
type aceHeader struct {
	Type  uint8
	Flags uint8
	Size  uint16
	Mask  uint32
}

func checkSecurity(path string, private bool) error {
	name, err := syscall.UTF16PtrFromString(path)
	if err != nil {
		return err
	}
	var owner *syscall.SID
	var acl *aclHeader
	var descriptor uintptr
	result, _, _ := securityInfo.Call(uintptr(unsafe.Pointer(name)), 1, 0x1|0x4, uintptr(unsafe.Pointer(&owner)), 0, uintptr(unsafe.Pointer(&acl)), 0, uintptr(unsafe.Pointer(&descriptor)))
	if result != 0 {
		return syscall.Errno(result)
	}
	defer syscall.LocalFree(syscall.Handle(descriptor))
	sid, err := currentSID()
	if err != nil {
		return err
	}
	ownerSID, err := owner.String()
	if err != nil {
		return err
	}
	if ownerSID != sid {
		return errors.New("path belongs to another user")
	}
	if !private {
		return nil
	}
	if acl == nil {
		return errors.New("operation path has unrestricted ACL")
	}
	for i := range acl.Count {
		var pointer uintptr
		ok, _, e := getAce.Call(uintptr(unsafe.Pointer(acl)), uintptr(i), uintptr(unsafe.Pointer(&pointer)))
		if ok == 0 {
			return e
		}
		ace := (*aceHeader)(unsafe.Pointer(pointer))
		if ace.Flags&0x08 != 0 {
			continue
		} // inherit-only ACE does not grant this object access
		if ace.Type == 1 {
			continue
		} // deny ACE can only restrict access
		if ace.Type != 0 {
			return errors.New("unsupported operation ACL entry")
		}
		granted, err := (*syscall.SID)(unsafe.Pointer(pointer + 8)).String()
		if err != nil {
			return err
		}
		// Private request/status includes a nonce. No unrelated principal may read,
		// write or change its ACL; SYSTEM and Administrators are OS trust boundaries.
		if granted != sid && granted != "S-1-5-18" && granted != "S-1-5-32-544" && ace.Mask != 0 {
			return errors.New("operation ACL permits another principal")
		}
	}
	return nil
}
func ownedPath(path string) error { return checkSecurity(path, false) }
func privatePath(path string, directory bool) error {
	if err := cleanPath(path); err != nil {
		return err
	}
	st, err := os.Lstat(path)
	if err != nil {
		return err
	}
	if st.IsDir() != directory || (!directory && !st.Mode().IsRegular()) {
		return errors.New("invalid private path type")
	}
	return checkSecurity(path, true)
}
func acquireLease(path string) (*os.File, error) {
	if _, err := os.Lstat(path); err == nil {
		if err = regular(path); err != nil {
			return nil, err
		}
	} else if !errors.Is(err, os.ErrNotExist) {
		return nil, err
	}
	f, err := os.OpenFile(path, os.O_CREATE|os.O_RDWR, 0600)
	if err != nil {
		return nil, err
	}
	var overlapped syscall.Overlapped
	ok, _, e := lockFile.Call(f.Fd(), 0x2|0x1, 0, 1, 0, uintptr(unsafe.Pointer(&overlapped)))
	if ok == 0 {
		f.Close()
		return nil, fmt.Errorf("another updater holds the update lease: %w", e)
	}
	return f, nil
}
func replaceFile(source, destination string) error {
	from, err := syscall.UTF16PtrFromString(source)
	if err != nil {
		return err
	}
	to, err := syscall.UTF16PtrFromString(destination)
	if err != nil {
		return err
	}
	ok, _, e := moveFile.Call(uintptr(unsafe.Pointer(from)), uintptr(unsafe.Pointer(to)), 0x1|0x8)
	if ok == 0 {
		return e
	}
	return nil
}

// MoveFileEx WRITE_THROUGH and File.Sync provide Windows durability; opening
// directories for FlushFileBuffers is not supported on all target filesystems.
func syncDirectory(path string) error { return nil }
func detach(cmd *exec.Cmd) {
	cmd.SysProcAttr = &syscall.SysProcAttr{HideWindow: true, CreationFlags: 0x8 | 0x200}
}
func hideWindow(cmd *exec.Cmd) {
	cmd.SysProcAttr = &syscall.SysProcAttr{HideWindow: true, CreationFlags: 0x08000000}
}
func validateAppImageProcess(p processInfo, image string) error {
	return errors.New("AppImage requires Linux")
}

package main

import (
	"crypto/rand"
	"encoding/hex"
	"errors"
	"os"
	"path/filepath"
	"runtime"
	"sync"
	"syscall"
	"unsafe"
)

var convertSecurityDescriptor = advapi32.NewProc("ConvertStringSecurityDescriptorToSecurityDescriptorW")

// Set the owner when creating the object. An elevated token may default to
// Administrators, not TokenUser. Leave the DACL to ordinary inheritance from the
// private parent, and never apply this descriptor to an existing object.
var creationDescriptor = sync.OnceValues(func() ([]byte, error) {
	sid, err := currentSID()
	if err != nil {
		return nil, err
	}
	return securityDescriptor("O:" + sid)
})

func securityDescriptor(sddl string) ([]byte, error) {
	text, err := syscall.UTF16PtrFromString(sddl)
	if err != nil {
		return nil, err
	}
	var descriptor *byte
	var size uint32
	ok, _, err := convertSecurityDescriptor.Call(uintptr(unsafe.Pointer(text)), 1, uintptr(unsafe.Pointer(&descriptor)), uintptr(unsafe.Pointer(&size)))
	if ok == 0 {
		return nil, err
	}
	defer syscall.LocalFree(syscall.Handle(uintptr(unsafe.Pointer(descriptor))))
	// The descriptor is self-relative; the copied bytes contain no pointers.
	return append([]byte(nil), unsafe.Slice(descriptor, size)...), nil
}

func creationAttributes(descriptor []byte) syscall.SecurityAttributes {
	return syscall.SecurityAttributes{
		Length:             uint32(unsafe.Sizeof(syscall.SecurityAttributes{})),
		SecurityDescriptor: uintptr(unsafe.Pointer(&descriptor[0])),
	}
}

func createOwnedFile(path string, access int, mode os.FileMode) (*os.File, error) {
	descriptor, err := creationDescriptor()
	if err != nil {
		return nil, err
	}
	name, err := syscall.UTF16PtrFromString(path)
	if err != nil {
		return nil, err
	}
	rights := uint32(syscall.GENERIC_WRITE)
	if access == os.O_RDWR {
		rights |= syscall.GENERIC_READ
	}
	flags := uint32(syscall.FILE_ATTRIBUTE_NORMAL | syscall.FILE_FLAG_OPEN_REPARSE_POINT)
	if mode&0200 == 0 {
		flags = syscall.FILE_ATTRIBUTE_READONLY | syscall.FILE_FLAG_OPEN_REPARSE_POINT
	}
	attributes := creationAttributes(descriptor)
	h, err := syscall.CreateFile(name, rights, syscall.FILE_SHARE_READ|syscall.FILE_SHARE_WRITE, &attributes, syscall.CREATE_NEW, flags, 0)
	runtime.KeepAlive(descriptor)
	if err != nil {
		return nil, &os.PathError{Op: "create", Path: path, Err: err}
	}
	return os.NewFile(uintptr(h), path), nil
}

func createOwnedTemp(directory string) (*os.File, error) {
	var suffix [8]byte
	if _, err := rand.Read(suffix[:]); err != nil {
		return nil, err
	}
	return createOwnedFile(filepath.Join(directory, ".write-"+hex.EncodeToString(suffix[:])), os.O_WRONLY, 0600)
}

func mkdirOwned(path string) error {
	descriptor, err := creationDescriptor()
	if err != nil {
		return err
	}
	return mkdirWithDescriptor(path, descriptor)
}

func mkdirWithDescriptor(path string, descriptor []byte) error {
	name, err := syscall.UTF16PtrFromString(path)
	if err != nil {
		return err
	}
	attributes := creationAttributes(descriptor)
	err = syscall.CreateDirectory(name, &attributes)
	runtime.KeepAlive(descriptor)
	if err != nil {
		return &os.PathError{Op: "mkdir", Path: path, Err: err}
	}
	return nil
}

// Archive entries can refer to implicit parents, which also need a user owner.
func mkdirAllOwned(path string) error {
	if st, err := os.Lstat(path); err == nil {
		if st.IsDir() {
			return nil
		}
		return &os.PathError{Op: "mkdir", Path: path, Err: syscall.ENOTDIR}
	} else if !errors.Is(err, os.ErrNotExist) {
		return err
	}
	if parent := filepath.Dir(path); parent != path {
		if err := mkdirAllOwned(parent); err != nil {
			return err
		}
	}
	return mkdirOwned(path)
}

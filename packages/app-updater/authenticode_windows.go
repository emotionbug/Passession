package main

import (
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"os"
	"runtime"
	"slices"
	"strings"
	"sync"
	"syscall"
	"unsafe"
)

const (
	trustNoSignature          = 0x800B0100 // TRUST_E_NOSIGNATURE
	wtdUINone                 = 2
	wtdRevokeNone             = 0
	wtdChoiceFile             = 1
	wtdChoiceCatalog          = 2
	wtdStateActionVerify      = 1
	wtdStateActionClose       = 2
	loadLibrarySearchSystem32 = 0x00000800
	errorNotFound             = syscall.Errno(1168)
	maxSignerCertificate      = 1 << 20
)

// WINTRUST_ACTION_GENERIC_VERIFY_V2, the Authenticode policy used by
// Get-AuthenticodeSignature.
var genericVerifyV2 = syscall.GUID{Data1: 0x00aac56b, Data2: 0xcd44, Data3: 0x11d0, Data4: [8]byte{0x8c, 0xc2, 0x00, 0xc0, 0x4f, 0xc2, 0x95, 0xee}}

var loadLibraryEx = kernel32.NewProc("LoadLibraryExW")

// WINTRUST_FILE_INFO
type winTrustFileInfo struct {
	Size         uint32
	FilePath     *uint16
	File         syscall.Handle
	KnownSubject *syscall.GUID
}

// WINTRUST_CATALOG_INFO, including the Windows 8 hCatAdmin member that
// selects the SHA256 catalog database.
type winTrustCatalogInfo struct {
	Size                   uint32
	CatalogVersion         uint32
	CatalogFilePath        *uint16
	MemberTag              *uint16
	MemberFilePath         *uint16
	MemberFile             syscall.Handle
	CalculatedFileHash     *byte
	CalculatedFileHashSize uint32
	CatalogContext         uintptr
	CatAdmin               uintptr
}

// CATALOG_INFO
type catalogInfo struct {
	Size uint32
	File [syscall.MAX_PATH]uint16
}

// WINTRUST_DATA; Choice is the pFile/pCatalog union member.
type winTrustData struct {
	Size               uint32
	PolicyCallbackData uintptr
	SIPClientData      uintptr
	UIChoice           uint32
	RevocationChecks   uint32
	UnionChoice        uint32
	Choice             unsafe.Pointer
	StateAction        uint32
	StateData          syscall.Handle
	URLReference       *uint16
	ProvFlags          uint32
	UIContext          uint32
	SignatureSettings  uintptr
}

// CRYPT_PROVIDER_CERT prefix; only the leaf certificate context is read.
type cryptProviderCert struct {
	Size uint32
	Cert *syscall.CertContext
}

type wintrustProcs struct {
	verify, providerData, providerSigner, providerCert                   *syscall.Proc
	acquireCatalogs, hashFile, findCatalog, releaseCatalog, releaseAdmin *syscall.Proc
	catalogInfo                                                          *syscall.Proc
}

// syscall.NewLazyDLL resolves DLLs Go itself does not use through the default
// search order, which includes the application and current directories. Load
// wintrust.dll and its dependencies from System32 only; the module stays
// loaded for the helper's lifetime.
var wintrust = sync.OnceValues(func() (*wintrustProcs, error) {
	name, err := syscall.UTF16PtrFromString("wintrust.dll")
	if err != nil {
		return nil, err
	}
	handle, _, e := loadLibraryEx.Call(uintptr(unsafe.Pointer(name)), 0, loadLibrarySearchSystem32)
	if handle == 0 {
		return nil, fmt.Errorf("load System32 wintrust.dll: %w", e)
	}
	dll := &syscall.DLL{Name: "wintrust.dll", Handle: syscall.Handle(handle)}
	p := new(wintrustProcs)
	for _, f := range []struct {
		proc **syscall.Proc
		name string
	}{
		{&p.verify, "WinVerifyTrust"},
		{&p.providerData, "WTHelperProvDataFromStateData"},
		{&p.providerSigner, "WTHelperGetProvSignerFromChain"},
		{&p.providerCert, "WTHelperGetProvCertFromChain"},
		{&p.acquireCatalogs, "CryptCATAdminAcquireContext2"},
		{&p.hashFile, "CryptCATAdminCalcHashFromFileHandle2"},
		{&p.findCatalog, "CryptCATAdminEnumCatalogFromHash"},
		{&p.releaseCatalog, "CryptCATAdminReleaseCatalogContext"},
		{&p.releaseAdmin, "CryptCATAdminReleaseContext"},
		{&p.catalogInfo, "CryptCATCatalogInfoFromContext"},
	} {
		if *f.proc, err = dll.FindProc(f.name); err != nil {
			return nil, err
		}
	}
	return p, nil
})

// inspectAuthenticode verifies a PE image's Authenticode signature with the
// default WinVerifyTrust policy, as Get-AuthenticodeSignature did, without
// loading PowerShell modules. A valid system catalog signature takes
// precedence. When a catalog lists the image but WinTrust rejects that
// catalog signature, only a fully valid embedded signature is accepted, never
// "unsigned". Lookup and API failures are errors.
func inspectAuthenticode(path string) (authenticodeIdentity, error) {
	name, err := syscall.UTF16PtrFromString(path)
	if err != nil {
		return authenticodeIdentity{}, err
	}
	// Deny writers so every check below reads the same bytes.
	handle, err := syscall.CreateFile(name, syscall.GENERIC_READ, syscall.FILE_SHARE_READ, nil, syscall.OPEN_EXISTING, syscall.FILE_ATTRIBUTE_NORMAL, 0)
	if err != nil {
		return authenticodeIdentity{}, err
	}
	file := os.NewFile(uintptr(handle), path)
	defer file.Close()
	st, err := file.Stat()
	if err != nil {
		return authenticodeIdentity{}, err
	}
	if !st.Mode().IsRegular() {
		return authenticodeIdentity{}, errors.New("signed image is not a regular file")
	}
	_, address, length, err := peSecurityDirectory(file, st.Size())
	if err != nil {
		return authenticodeIdentity{}, err
	}
	procs, err := wintrust()
	if err != nil {
		return authenticodeIdentity{}, err
	}
	var catalogStatus uint32
	for _, algorithm := range []string{"SHA256", "SHA1"} {
		found, status, subject, err := verifyCatalogSignature(procs, name, file, algorithm)
		if err != nil {
			return authenticodeIdentity{}, fmt.Errorf("%s catalog signature: %w", algorithm, err)
		}
		if found && status == 0 {
			return authenticodeIdentity{signed: true, subject: subject}, nil
		}
		if found {
			catalogStatus = status
			break
		}
	}
	if _, err = file.Seek(0, io.SeekStart); err != nil {
		return authenticodeIdentity{}, err
	}
	info := &winTrustFileInfo{FilePath: name, File: syscall.Handle(file.Fd())}
	info.Size = uint32(unsafe.Sizeof(*info))
	status, subject, err := verifyTrust(procs, wtdChoiceFile, unsafe.Pointer(info))
	runtime.KeepAlive(name)
	runtime.KeepAlive(file)
	if err != nil {
		return authenticodeIdentity{}, err
	}
	switch {
	case status == 0:
		return authenticodeIdentity{signed: true, subject: subject}, nil
	case catalogStatus != 0:
		return authenticodeIdentity{}, fmt.Errorf("WinVerifyTrust rejected the catalog signature (0x%08X) and the embedded signature (0x%08X)", catalogStatus, status)
	case status != trustNoSignature:
		return authenticodeIdentity{}, fmt.Errorf("WinVerifyTrust rejected the embedded signature (0x%08X)", status)
	case address != 0 || length != 0:
		// The image declares a certificate table WinTrust could not use.
		return authenticodeIdentity{}, errors.New("unverifiable embedded certificate table")
	}
	return authenticodeIdentity{}, nil
}

// verifyTrust runs WinVerifyTrust for one union choice and returns its status
// and, on success, the verified leaf signer's subject. The state is always
// closed; the caller keeps memory referenced by choice alive until return.
func verifyTrust(p *wintrustProcs, union uint32, choice unsafe.Pointer) (uint32, []byte, error) {
	data := &winTrustData{UIChoice: wtdUINone, RevocationChecks: wtdRevokeNone, UnionChoice: union, Choice: choice, StateAction: wtdStateActionVerify}
	data.Size = uint32(unsafe.Sizeof(*data))
	result, _, _ := p.verify.Call(uintptr(syscall.InvalidHandle), uintptr(unsafe.Pointer(&genericVerifyV2)), uintptr(unsafe.Pointer(data)))
	defer func() {
		data.StateAction = wtdStateActionClose
		p.verify.Call(uintptr(syscall.InvalidHandle), uintptr(unsafe.Pointer(&genericVerifyV2)), uintptr(unsafe.Pointer(data)))
		runtime.KeepAlive(data)
	}()
	// WinVerifyTrust returns a 32-bit LONG; the upper register half is undefined.
	status := uint32(result)
	if status != 0 {
		return status, nil, nil
	}
	subject, err := verifiedSignerSubject(p, data.StateData)
	return status, subject, err
}

func verifiedSignerSubject(p *wintrustProcs, state syscall.Handle) ([]byte, error) {
	provider, _, _ := p.providerData.Call(uintptr(state))
	if provider == 0 {
		return nil, errors.New("verified signature has no provider data")
	}
	signer, _, _ := p.providerSigner.Call(provider, 0, 0, 0)
	if signer == 0 {
		return nil, errors.New("verified signature has no signer")
	}
	leaf, _, _ := p.providerCert.Call(signer, 0)
	if leaf == 0 {
		return nil, errors.New("verified signer has no certificate")
	}
	cert := (*cryptProviderCert)(unsafe.Pointer(leaf))
	if cert.Size < uint32(unsafe.Sizeof(*cert)) || cert.Cert == nil || cert.Cert.EncodedCert == nil || cert.Cert.Length == 0 || cert.Cert.Length > maxSignerCertificate {
		return nil, errors.New("verified signer certificate is malformed")
	}
	// certificateSubject copies the Name before the trust state is closed.
	return certificateSubject(unsafe.Slice(cert.Cert.EncodedCert, cert.Cert.Length))
}

// verifyCatalogSignature looks the image up in the system catalogs for one
// hash algorithm and, when a catalog lists it, returns the WinVerifyTrust
// catalog status and, on success, the verified leaf signer's subject.
func verifyCatalogSignature(p *wintrustProcs, name *uint16, file *os.File, algorithm string) (found bool, status uint32, subject []byte, err error) {
	algorithmName, err := syscall.UTF16PtrFromString(algorithm)
	if err != nil {
		return false, 0, nil, err
	}
	var admin uintptr
	ok, _, e := p.acquireCatalogs.Call(uintptr(unsafe.Pointer(&admin)), 0, uintptr(unsafe.Pointer(algorithmName)), 0, 0)
	if uint32(ok) == 0 {
		return false, 0, nil, fmt.Errorf("open catalog database: %w", e)
	}
	defer p.releaseAdmin.Call(admin, 0)
	if _, err = file.Seek(0, io.SeekStart); err != nil {
		return false, 0, nil, err
	}
	hash := make([]byte, 64)
	size := uint32(len(hash))
	ok, _, e = p.hashFile.Call(admin, file.Fd(), uintptr(unsafe.Pointer(&size)), uintptr(unsafe.Pointer(&hash[0])), 0)
	if uint32(ok) == 0 {
		return false, 0, nil, fmt.Errorf("hash image: %w", e)
	}
	if size == 0 || size > uint32(len(hash)) {
		return false, 0, nil, errors.New("invalid catalog hash length")
	}
	catalogContext, _, e := p.findCatalog.Call(admin, uintptr(unsafe.Pointer(&hash[0])), uintptr(size), 0, 0)
	if catalogContext == 0 {
		// NULL alone does not distinguish "no catalog lists this hash" from a
		// failed search; only ERROR_NOT_FOUND is treated as absence.
		if errno, _ := e.(syscall.Errno); errno != errorNotFound {
			return false, 0, nil, fmt.Errorf("search catalogs: %w", e)
		}
		return false, 0, nil, nil
	}
	defer p.releaseCatalog.Call(admin, catalogContext, 0)
	catalog := new(catalogInfo)
	catalog.Size = uint32(unsafe.Sizeof(*catalog))
	if ok, _, e = p.catalogInfo.Call(catalogContext, uintptr(unsafe.Pointer(catalog)), 0); uint32(ok) == 0 {
		return true, 0, nil, fmt.Errorf("read catalog path: %w", e)
	}
	if catalog.File[0] == 0 || !slices.Contains(catalog.File[:], 0) {
		return true, 0, nil, errors.New("malformed catalog path")
	}
	// Catalog members are tagged with the uppercase hexadecimal file hash.
	tag, err := syscall.UTF16PtrFromString(strings.ToUpper(hex.EncodeToString(hash[:size])))
	if err != nil {
		return true, 0, nil, err
	}
	if _, err = file.Seek(0, io.SeekStart); err != nil {
		return true, 0, nil, err
	}
	member := &winTrustCatalogInfo{
		CatalogFilePath:        &catalog.File[0],
		MemberTag:              tag,
		MemberFilePath:         name,
		MemberFile:             syscall.Handle(file.Fd()),
		CalculatedFileHash:     &hash[0],
		CalculatedFileHashSize: size,
		CatAdmin:               admin,
	}
	member.Size = uint32(unsafe.Sizeof(*member))
	status, subject, err = verifyTrust(p, wtdChoiceCatalog, unsafe.Pointer(member))
	runtime.KeepAlive(catalog)
	runtime.KeepAlive(tag)
	runtime.KeepAlive(hash)
	runtime.KeepAlive(name)
	runtime.KeepAlive(file)
	if err != nil {
		return true, 0, nil, err
	}
	return true, status, subject, nil
}

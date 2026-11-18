package main

import (
	"bytes"
	"encoding/binary"
	"os"
	"path/filepath"
	"strings"
	"syscall"
	"testing"
	"unsafe"
)

// WinTrust reads these structures through raw pointers; their Go layout must
// match the 64-bit C ABI.
func TestWinTrustStructuresMatchNativeLayout(t *testing.T) {
	if unsafe.Sizeof(uintptr(0)) != 8 {
		t.Skip("layout constants are for 64-bit Windows")
	}
	var data winTrustData
	var info winTrustFileInfo
	var member winTrustCatalogInfo
	var catalog catalogInfo
	var cert cryptProviderCert
	for name, c := range map[string][2]uintptr{
		"WINTRUST_DATA size":                         {unsafe.Sizeof(data), 88},
		"WINTRUST_DATA.pFile":                        {unsafe.Offsetof(data.Choice), 40},
		"WINTRUST_DATA.hWVTStateData":                {unsafe.Offsetof(data.StateData), 56},
		"WINTRUST_DATA.pSignatureSettings":           {unsafe.Offsetof(data.SignatureSettings), 80},
		"WINTRUST_FILE_INFO size":                    {unsafe.Sizeof(info), 32},
		"WINTRUST_CATALOG_INFO size":                 {unsafe.Sizeof(member), 72},
		"WINTRUST_CATALOG_INFO.hMemberFile":          {unsafe.Offsetof(member.MemberFile), 32},
		"WINTRUST_CATALOG_INFO.cbCalculatedFileHash": {unsafe.Offsetof(member.CalculatedFileHashSize), 48},
		"WINTRUST_CATALOG_INFO.hCatAdmin":            {unsafe.Offsetof(member.CatAdmin), 64},
		"CATALOG_INFO size":                          {unsafe.Sizeof(catalog), 524},
		"CRYPT_PROVIDER_CERT.pCert":                  {unsafe.Offsetof(cert.Cert), 8},
	} {
		if c[0] != c[1] {
			t.Errorf("%s = %d, want %d", name, c[0], c[1])
		}
	}
}

// Inbox Windows executables are catalog-signed; they must verify through the
// catalog signer rather than fail closed or pass as unsigned.
func TestNativeAuthenticodeVerifiesInboxCatalogSigner(t *testing.T) {
	t.Setenv("PSModulePath", filepath.Join(t.TempDir(), "PowerShell", "7", "Modules"))
	system := os.Getenv("SystemRoot")
	if system == "" {
		t.Fatal("SystemRoot missing")
	}
	inbox := filepath.Join(system, "System32", "cmd.exe")
	identity, err := inspectAuthenticode(inbox)
	if err != nil || !identity.signed || !bytes.Contains(identity.subject, []byte("Microsoft")) {
		t.Fatalf("inbox image: signed=%v subject=%q, %v", identity.signed, identity.subject, err)
	}
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	// The unsigned test executable cannot replace a signed installation, but a
	// signed image may replace an unsigned one.
	if err = verifyPublisherContinuity(inbox, executable, "installer", "installed app"); err == nil || !strings.Contains(err.Error(), "installer publisher changed") {
		t.Fatalf("signed to unsigned: %v", err)
	}
	if err = verifyPublisherContinuity(executable, inbox, "installer", "installed app"); err != nil {
		t.Fatalf("unsigned to signed: %v", err)
	}
}

func TestNativeAuthenticodeClassifiesUnsignedAndTamperedImages(t *testing.T) {
	// A PowerShell 7 module path inherited by the helper broke Windows
	// PowerShell's Microsoft.PowerShell.Security; verification no longer loads it.
	poisoned := filepath.Join(t.TempDir(), "PowerShell", "7", "Modules")
	t.Setenv("PSModulePath", poisoned+string(os.PathListSeparator)+os.Getenv("PSModulePath"))
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	// go test binaries carry no Authenticode signature and are in no catalog.
	image, err := os.ReadFile(executable)
	if err != nil {
		t.Fatal(err)
	}
	directory := t.TempDir()
	unsigned := filepath.Join(directory, "unsigned.exe")
	if err = os.WriteFile(unsigned, image, 0600); err != nil {
		t.Fatal(err)
	}
	identity, err := inspectAuthenticode(unsigned)
	if err != nil || identity.signed {
		t.Fatalf("unsigned image: %+v, %v", identity, err)
	}
	if err = verifyPublisherContinuity(unsigned, unsigned, "installer", "installed app"); err != nil {
		t.Fatal(err)
	}

	// Declare a certificate table holding a WIN_CERTIFICATE with no valid
	// PKCS#7 signature: an invalid signature, never an unsigned image.
	entry, _, _, err := peSecurityDirectory(bytes.NewReader(image), int64(len(image)))
	if err != nil || entry < 0 {
		t.Fatalf("test executable security directory: %d, %v", entry, err)
	}
	tampered := bytes.Clone(image)
	for len(tampered)%8 != 0 {
		tampered = append(tampered, 0)
	}
	offset := len(tampered)
	certificate := bytes.Repeat([]byte{0xa5}, 32)
	binary.LittleEndian.PutUint32(certificate, uint32(len(certificate)))
	binary.LittleEndian.PutUint16(certificate[4:], 0x0200) // WIN_CERT_REVISION_2_0
	binary.LittleEndian.PutUint16(certificate[6:], 0x0002) // WIN_CERT_TYPE_PKCS_SIGNED_DATA
	tampered = append(tampered, certificate...)
	binary.LittleEndian.PutUint32(tampered[entry:], uint32(offset))
	binary.LittleEndian.PutUint32(tampered[entry+4:], uint32(len(certificate)))
	invalid := filepath.Join(directory, "tampered.exe")
	if err = os.WriteFile(invalid, tampered, 0600); err != nil {
		t.Fatal(err)
	}
	if identity, err = inspectAuthenticode(invalid); err == nil {
		t.Fatalf("tampered image classified as %+v", identity)
	}
	if err = verifyPublisherContinuity(unsigned, invalid, "installer", "installed app"); err == nil || !strings.Contains(err.Error(), "invalid installer signature") {
		t.Fatalf("tampered candidate: %v", err)
	}
	if err = verifyPublisherContinuity(invalid, unsigned, "installer", "installed app"); err == nil || !strings.Contains(err.Error(), "invalid installed app signature") {
		t.Fatalf("tampered installation: %v", err)
	}
}

//! Native Windows Authenticode and directory ACL checks.
//!
//! These replace Windows PowerShell `Get-AuthenticodeSignature`, `Get-Acl` and
//! `Set-Acl`: a child `powershell.exe` inherits `PSModulePath` from the caller,
//! so a daemon started from PowerShell 7 made 5.1 load PS7's
//! `Microsoft.PowerShell.Security` and fail. Every function blocks (WinVerifyTrust
//! may retrieve certificate or timestamp data), so async callers run them on a
//! blocking thread.

use anyhow::{Context, Result, bail, ensure};
use std::{
    ffi::c_void,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    ptr::{self, null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{
        CERT_E_CHAINING, CERT_E_EXPIRED, CERT_E_REVOKED, CERT_E_UNTRUSTEDROOT,
        CERT_E_UNTRUSTEDTESTROOT, CRYPT_E_FILE_ERROR, CRYPT_E_REVOKED, CRYPT_E_SECURITY_SETTINGS,
        ERROR_INSUFFICIENT_BUFFER, ERROR_NOT_FOUND, ERROR_SUCCESS, GetLastError, HANDLE, LocalFree,
        TRUST_E_BAD_DIGEST, TRUST_E_CERT_SIGNATURE, TRUST_E_EXPLICIT_DISTRUST,
        TRUST_E_MALFORMED_SIGNATURE, TRUST_E_NO_SIGNER_CERT, TRUST_E_NOSIGNATURE,
        TRUST_E_SUBJECT_FORM_UNKNOWN, TRUST_E_SUBJECT_NOT_TRUSTED, TRUST_E_TIME_STAMP,
    },
    Security::{
        ACE_HEADER, ACL, ACL_REVISION, AddAccessAllowedAceEx,
        Authorization::{
            GetNamedSecurityInfoW, GetSecurityInfo, SE_FILE_OBJECT, SE_KERNEL_OBJECT,
            SetNamedSecurityInfoW,
        },
        CONTAINER_INHERIT_ACE, CopySid, CreateWellKnownSid,
        Cryptography::Catalog::{
            CATALOG_INFO, CryptCATAdminAcquireContext2, CryptCATAdminCalcHashFromFileHandle2,
            CryptCATAdminEnumCatalogFromHash, CryptCATAdminReleaseCatalogContext,
            CryptCATAdminReleaseContext, CryptCATCatalogInfoFromContext,
        },
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetLengthSid, GetSecurityDescriptorLength,
        GetTokenInformation, InitializeAcl, InitializeSecurityDescriptor, IsValidSid,
        OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSID,
        SE_DACL_PROTECTED, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SECURITY_MAX_SID_SIZE,
        SetSecurityDescriptorControl, SetSecurityDescriptorDacl, SetSecurityDescriptorOwner,
        TOKEN_QUERY, TOKEN_USER, TokenUser, WELL_KNOWN_SID_TYPE, WinLocalSystemSid,
        WinTrust::{
            WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_CATALOG_INFO, WINTRUST_DATA,
            WINTRUST_FILE_INFO, WTD_CHOICE_CATALOG, WTD_CHOICE_FILE,
            WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE,
            WTHelperGetProvCertFromChain, WTHelperGetProvSignerFromChain,
            WTHelperProvDataFromStateData, WinVerifyTrust,
        },
    },
    Storage::FileSystem::{FILE_ALL_ACCESS, FILE_SHARE_READ},
    System::{
        SystemServices::SECURITY_DESCRIPTOR_REVISION,
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

/// Whether an unsigned installation may move to a signed candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnsignedPolicy {
    AllowSignedUpgrade,
    PreserveUnsigned,
}

/// Accounts a private directory's DACL may grant access to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AllowedDirectoryUsers {
    CurrentUser,
    CurrentUserAndSystem,
}

/// Requires the candidate to keep the installed executable's publisher.
///
/// A validly signed installation requires a validly signed candidate whose leaf
/// signer certificate Subject has identical DER bytes. A genuinely unsigned
/// installation accepts an unsigned candidate, and a valid signed one only under
/// [`UnsignedPolicy::AllowSignedUpgrade`]. Invalid, untrusted, tampered and
/// unreadable signatures fail; a failed catalog can be recovered only by a
/// fully valid embedded signature.
pub fn verify_update_signature(
    installed: &Path,
    candidate: &Path,
    policy: UnsignedPolicy,
) -> Result<()> {
    let installed = authenticode(installed).context("Installed Authenticode signature is invalid")?;
    let candidate = authenticode(candidate).context("Invalid update Authenticode signature")?;
    update_continuity(&installed, &candidate, policy)
}

/// Accepts a valid catalog or embedded Authenticode signature, or a genuinely
/// unsigned file.
pub fn verify_local_signature(executable: &Path) -> Result<()> {
    authenticode(executable).context("Invalid local runtime Authenticode signature")?;
    Ok(())
}

/// Requires the current user to own `path` and every allow ACE in its DACL,
/// inherited or explicit, to name an allowed account. Deny ACEs only remove
/// access and are accepted.
pub fn verify_private_directory(path: &Path, allowed: AllowedDirectoryUsers) -> Result<()> {
    let user = current_user_sid()?;
    let mut sids = vec![user];
    if allowed == AllowedDirectoryUsers::CurrentUserAndSystem {
        sids.push(well_known_sid(WinLocalSystemSid)?);
    }
    let path_wide = wide(path)?;
    let mut owner: PSID = null_mut();
    let mut dacl: *mut ACL = null_mut();
    let mut descriptor = null_mut();
    // SAFETY: valid NUL-terminated path and out-pointers; the descriptor is
    // released by `LocalAllocation`, and `owner`/`dacl` point into it.
    let status = unsafe {
        GetNamedSecurityInfoW(
            path_wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    let descriptor = LocalAllocation(descriptor);
    ensure!(
        status == ERROR_SUCCESS,
        "Cannot read the ACL of {}: {}",
        path.display(),
        std::io::Error::from_raw_os_error(status as i32)
    );
    verify_private_descriptor(owner, dacl, &descriptor, &sids)
        .with_context(|| format!("{} is not private to the current user", path.display()))
}

fn verify_private_descriptor(
    owner: PSID,
    dacl: *mut ACL,
    descriptor: &LocalAllocation,
    allowed: &[Sid],
) -> Result<()> {
    ensure!(!descriptor.0.is_null(), "Missing security descriptor");
    // SAFETY: Windows returned a self-relative descriptor.
    let length = unsafe { GetSecurityDescriptorLength(descriptor.0) } as usize;
    let start = descriptor.0 as usize;
    let end = start.checked_add(length).context("Malformed security descriptor")?;
    let within = |pointer: usize| pointer >= start && pointer < end;
    ensure!(!owner.is_null() && within(owner as usize), "Directory owner is unavailable");
    // SAFETY: `owner` lies inside the descriptor; `sid_length` bounds the read.
    unsafe { sid_length(owner as *const u8, end - owner as usize) }?;
    ensure!(
        // SAFETY: both SIDs were validated.
        unsafe { EqualSid(owner, allowed[0].as_psid()) } != 0,
        "Object is not owned by the current user",
    );
    ensure!(!dacl.is_null(), "Object has an unrestricted (NULL) DACL");
    ensure!(within(dacl as usize), "DACL lies outside its descriptor");
    // SAFETY: `dacl` lies inside the descriptor and is bounded by its end.
    unsafe { check_dacl(dacl, end - dacl as usize, allowed) }
}

/// Sets the current user as owner and replaces the DACL with one protected
/// (non-inheriting) full-control entry for the current user that containers and
/// files inherit. Windows propagates the inheritable entry to existing children.
pub fn restrict_directory_to_current_user(path: &Path) -> Result<()> {
    with_current_user_acl(CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE, |user, acl| {
        let path_wide = wide(path)?;
        // SAFETY: valid NUL-terminated path, SID and ACL.
        let status = unsafe {
            SetNamedSecurityInfoW(
                path_wide.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION,
                user.as_psid(),
                null_mut(),
                acl,
                null(),
            )
        };
        ensure!(
            status == ERROR_SUCCESS,
            "Cannot restrict {} to the current user: {}",
            path.display(),
            std::io::Error::from_raw_os_error(status as i32)
        );
        Ok(())
    })
}

fn with_current_user_acl<T>(
    inheritance: u32,
    apply: impl FnOnce(&Sid, *mut ACL) -> Result<T>,
) -> Result<T> {
    let user = current_user_sid()?;
    let acl_length = size_of::<ACL>() + ACCESS_ACE_SID_OFFSET + user.byte_len();
    let mut storage = vec![0u32; acl_length.div_ceil(size_of::<u32>())];
    let acl_bytes = u32::try_from(storage.len() * size_of::<u32>())?;
    let acl = storage.as_mut_ptr().cast::<ACL>();
    // SAFETY: `acl` is a DWORD-aligned buffer of `acl_bytes` bytes that outlives
    // every use; `user` is a valid SID.
    unsafe {
        ensure!(
            InitializeAcl(acl, acl_bytes, ACL_REVISION) != 0,
            "InitializeAcl failed: {}",
            std::io::Error::last_os_error()
        );
        ensure!(
            AddAccessAllowedAceEx(
                acl,
                ACL_REVISION,
                inheritance,
                FILE_ALL_ACCESS,
                user.as_psid(),
            ) != 0,
            "AddAccessAllowedAceEx failed: {}",
            std::io::Error::last_os_error()
        );
    }
    apply(&user, acl)
}

pub(crate) fn create_private_named_pipe(
    options: &tokio::net::windows::named_pipe::ServerOptions,
    name: &str,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    with_current_user_acl(0, |user, acl| {
        let mut descriptor = SECURITY_DESCRIPTOR::default();
        let descriptor = (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast::<c_void>();
        // SAFETY: the descriptor is initialized before use and borrows the
        // current-user SID and DACL only until the kernel creates the pipe.
        unsafe {
            if InitializeSecurityDescriptor(descriptor, SECURITY_DESCRIPTOR_REVISION) == 0
                || SetSecurityDescriptorOwner(descriptor, user.as_psid(), 0) == 0
                || SetSecurityDescriptorDacl(descriptor, 1, acl, 0) == 0
                || SetSecurityDescriptorControl(descriptor, SE_DACL_PROTECTED, SE_DACL_PROTECTED)
                    == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut attributes = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            };
            Ok(options.create_with_security_attributes_raw(
                name,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )?)
        }
    })
}

pub(crate) fn verify_private_named_pipe(
    pipe: &tokio::net::windows::named_pipe::NamedPipeClient,
) -> Result<()> {
    let user = current_user_sid()?;
    let mut owner = null_mut();
    let mut dacl = null_mut();
    let mut descriptor = null_mut();
    // SAFETY: use the connected pipe handle, not its name; returned pointers
    // are backed by the descriptor allocation below.
    let status = unsafe {
        GetSecurityInfo(
            pipe.as_raw_handle() as HANDLE,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    let descriptor = LocalAllocation(descriptor);
    if status != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(status as i32).into());
    }
    verify_private_descriptor(owner, dacl, &descriptor, &[user])
}

#[derive(Debug, Eq, PartialEq)]
enum Authenticode {
    /// DER of the leaf signer certificate's Subject name.
    Signed { subject: Vec<u8> },
    Unsigned,
}

fn update_continuity(
    installed: &Authenticode,
    candidate: &Authenticode,
    policy: UnsignedPolicy,
) -> Result<()> {
    match (installed, candidate) {
        (Authenticode::Signed { subject: old }, Authenticode::Signed { subject: new }) => {
            ensure!(old == new, "Update publisher mismatch")
        }
        (Authenticode::Signed { .. }, Authenticode::Unsigned) => {
            bail!("Update publisher mismatch: the update is unsigned")
        }
        (Authenticode::Unsigned, Authenticode::Unsigned) => {}
        (Authenticode::Unsigned, Authenticode::Signed { .. }) => ensure!(
            policy == UnsignedPolicy::AllowSignedUpgrade,
            "Update publisher mismatch: the unsigned installation must stay unsigned"
        ),
    }
    Ok(())
}

// winnt.h ACE types (System_SystemServices is not enabled).
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_DENIED_ACE_TYPE: u8 = 1;
/// ACE_HEADER plus ACCESS_MASK precede the SID in ACCESS_ALLOWED/DENIED_ACE.
const ACCESS_ACE_SID_OFFSET: usize = size_of::<ACE_HEADER>() + size_of::<u32>();
const SID_MAX_SUB_AUTHORITIES: usize = 15;
const MAX_SUBJECT_DER: usize = 64 * 1024;
const MAX_TOKEN_USER: u32 = 4096;
const MAX_CATALOG_HASH: u32 = 64;

/// Classifies `path` following PowerShell's `GetSignature`: a valid catalog
/// signature takes precedence, and a listing catalog that fails verification
/// falls back to the embedded signature. Both use
/// WINTRUST_ACTION_GENERIC_VERIFY_V2 with WTD_UI_NONE and no revocation or
/// provider flags.
///
/// A file is unsigned only when no catalog lists its hash and its PE header
/// declares no certificate table. After a failed catalog only a valid embedded
/// signature is accepted, never "unsigned". Catalog lookup and API failures
/// are errors.
fn authenticode(path: &Path) -> Result<Authenticode> {
    // Read-only sharing keeps the bytes behind every check below fixed.
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
        .with_context(|| format!("Cannot open {}", path.display()))?;
    let path_wide = wide(path)?;
    match catalog_signature(&file, &path_wide, path)? {
        CatalogSignature::Valid(subject) => Ok(Authenticode::Signed { subject }),
        CatalogSignature::Absent => embedded_signature(&file, &path_wide, path),
        CatalogSignature::Failed(catalog) => match embedded_signature(&file, &path_wide, path) {
            Ok(signed @ Authenticode::Signed { .. }) => Ok(signed),
            Ok(Authenticode::Unsigned) => {
                Err(catalog.context("No embedded signature to fall back to"))
            }
            Err(embedded) => Err(catalog.context(format!(
                "The embedded signature fallback also failed: {embedded:#}"
            ))),
        },
    }
}

fn embedded_signature(mut file: &File, path_wide: &[u16], path: &Path) -> Result<Authenticode> {
    file.seek(SeekFrom::Start(0))?;
    // SAFETY: `path_wide` and `file` outlive the returned state.
    let trust = unsafe {
        TrustState::verify(TrustSubject::File(WINTRUST_FILE_INFO {
            cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
            pcwszFilePath: path_wide.as_ptr(),
            hFile: file.as_raw_handle() as HANDLE,
            pgKnownSubject: null_mut(),
        }))
    };
    match trust.status {
        0 => Ok(Authenticode::Signed {
            subject: signer_subject(&trust).context("Unreadable Authenticode signer")?,
        }),
        // TRUST_E_NOSIGNATURE also covers a declared certificate table the PE
        // SIP cannot read, so absence is decided from the header itself.
        TRUST_E_NOSIGNATURE => {
            let entry = certificate_table_entry(file)
                .with_context(|| format!("Cannot read the PE header of {}", path.display()))?;
            ensure!(
                entry.is_none_or(|entry| entry.address == 0 && entry.size == 0),
                "{} declares an Authenticode certificate table that cannot be verified (WinVerifyTrust 0x{:08X}, last error 0x{:08X})",
                path.display(),
                trust.status as u32,
                trust.last_error
            );
            Ok(Authenticode::Unsigned)
        }
        status => bail!(
            "{}: {} (WinVerifyTrust 0x{:08X})",
            path.display(),
            trust_failure(status),
            status as u32
        ),
    }
}

/// IMAGE_DIRECTORY_ENTRY_SECURITY; `address` is a file offset, not an RVA.
#[derive(Debug, Eq, PartialEq)]
struct CertificateTableEntry {
    /// File offset of the data-directory entry itself.
    offset: u64,
    address: u32,
    size: u32,
}

/// Reads the PE certificate-table data directory. `None` means the optional
/// header has too few data directories to declare one; anything that is not a
/// well-formed PE32/PE32+ header is an error.
fn certificate_table_entry(mut image: impl Read + Seek) -> Result<Option<CertificateTableEntry>> {
    const MALFORMED: &str = "Not a well-formed PE image";
    const SECURITY_DIRECTORY: u64 = 4;
    fn read<const N: usize>(image: &mut (impl Read + Seek), offset: u64) -> Result<[u8; N]> {
        let mut bytes = [0u8; N];
        image.seek(SeekFrom::Start(offset))?;
        image.read_exact(&mut bytes).context(MALFORMED)?;
        Ok(bytes)
    }
    let dos = read::<64>(&mut image, 0)?;
    ensure!(&dos[..2] == b"MZ", MALFORMED);
    let pe = u64::from(u32::from_le_bytes([dos[60], dos[61], dos[62], dos[63]]));
    // Signature, COFF file header, optional-header magic.
    let headers = read::<26>(&mut image, pe)?;
    ensure!(&headers[..4] == b"PE\0\0", MALFORMED);
    let optional_size = u64::from(u16::from_le_bytes([headers[20], headers[21]]));
    let (count_offset, directories) = match u16::from_le_bytes([headers[24], headers[25]]) {
        0x10b => (92, 96),
        0x20b => (108, 112),
        _ => bail!(MALFORMED),
    };
    ensure!(optional_size >= directories, MALFORMED);
    let optional = pe + 24;
    ensure!(optional + optional_size <= image.seek(SeekFrom::End(0))?, MALFORMED);
    let count = u64::from(u32::from_le_bytes(read::<4>(&mut image, optional + count_offset)?));
    if count <= SECURITY_DIRECTORY {
        return Ok(None);
    }
    let entry = directories + SECURITY_DIRECTORY * 8;
    ensure!(entry + 8 <= optional_size, MALFORMED);
    let bytes = read::<8>(&mut image, optional + entry)?;
    Ok(Some(CertificateTableEntry {
        offset: optional + entry,
        address: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        size: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
    }))
}

enum CatalogSignature {
    /// No catalog lists the file.
    Absent,
    /// Leaf signer Subject DER of the listing catalog.
    Valid(Vec<u8>),
    /// A catalog lists the file but WinVerifyTrust rejected it.
    Failed(anyhow::Error),
}

/// Verifies the system catalog that lists `file`, searching SHA256 catalogs
/// before legacy SHA1 ones and taking the first catalog Windows returns.
/// Lookup and API failures are errors rather than [`CatalogSignature::Failed`].
fn catalog_signature(mut file: &File, path_wide: &[u16], path: &Path) -> Result<CatalogSignature> {
    for algorithm in [windows_sys::core::w!("SHA256"), windows_sys::core::w!("SHA1")] {
        let mut admin = 0isize;
        // SAFETY: out-pointer only; null selects the default catalog subsystem.
        ensure!(
            unsafe { CryptCATAdminAcquireContext2(&mut admin, null(), algorithm, null(), 0) } != 0,
            "Cannot open the catalog database: {}",
            std::io::Error::last_os_error()
        );
        let admin = CatalogAdmin(admin);
        file.seek(SeekFrom::Start(0))?;
        let mut hash = [0u8; MAX_CATALOG_HASH as usize];
        let mut hash_length = MAX_CATALOG_HASH;
        // SAFETY: `hash` holds `hash_length` writable bytes.
        ensure!(
            unsafe {
                CryptCATAdminCalcHashFromFileHandle2(
                    admin.0,
                    file.as_raw_handle() as HANDLE,
                    &mut hash_length,
                    hash.as_mut_ptr(),
                    0,
                )
            } != 0
                && (1..=MAX_CATALOG_HASH).contains(&hash_length),
            "Cannot hash {} for catalog lookup: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
        // SAFETY: `hash_length` bytes of `hash` were written.
        let handle = unsafe {
            CryptCATAdminEnumCatalogFromHash(admin.0, hash.as_ptr(), hash_length, 0, null_mut())
        };
        if handle == 0 {
            // SAFETY: read immediately after the failed lookup.
            let error = unsafe { GetLastError() };
            ensure!(
                error == ERROR_NOT_FOUND,
                "Catalog lookup for {} failed: {}",
                path.display(),
                std::io::Error::from_raw_os_error(error as i32)
            );
            continue;
        }
        let context = CatalogContext {
            admin: admin.0,
            handle,
        };
        let mut info = CATALOG_INFO {
            cbStruct: size_of::<CATALOG_INFO>() as u32,
            wszCatalogFile: [0; 260],
        };
        // SAFETY: `info` is a correctly sized out-structure.
        ensure!(
            unsafe { CryptCATCatalogInfoFromContext(context.handle, &mut info, 0) } != 0,
            "Cannot resolve the catalog listing {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
        let terminator = info
            .wszCatalogFile
            .iter()
            .position(|unit| *unit == 0)
            .filter(|&end| end > 0)
            .context("Malformed catalog path")?;
        let catalog_path = &info.wszCatalogFile[..=terminator];
        // Hash-indexed catalogs tag each member with its uppercase hex hash.
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        let member_tag = hash[..hash_length as usize]
            .iter()
            .flat_map(|byte| [HEX[usize::from(byte >> 4)], HEX[usize::from(byte & 0x0f)]])
            .map(u16::from)
            .chain([0])
            .collect::<Vec<u16>>();
        file.seek(SeekFrom::Start(0))?;
        // SAFETY: the catalog path, member tag, file path, file handle, hash
        // and catalog admin context all outlive the returned state.
        let trust = unsafe {
            TrustState::verify(TrustSubject::Catalog(WINTRUST_CATALOG_INFO {
                cbStruct: size_of::<WINTRUST_CATALOG_INFO>() as u32,
                dwCatalogVersion: 0,
                pcwszCatalogFilePath: catalog_path.as_ptr(),
                pcwszMemberTag: member_tag.as_ptr(),
                pcwszMemberFilePath: path_wide.as_ptr(),
                hMemberFile: file.as_raw_handle() as HANDLE,
                pbCalculatedFileHash: hash.as_mut_ptr(),
                cbCalculatedFileHash: hash_length,
                pcCatalogContext: null_mut(),
                hCatAdmin: admin.0,
            }))
        };
        if trust.status != 0 {
            let catalog_name = String::from_utf16_lossy(&catalog_path[..terminator]);
            return Ok(CatalogSignature::Failed(anyhow::anyhow!(
                "{} is listed in catalog {catalog_name}, which failed verification: {} (WinVerifyTrust 0x{:08X})",
                path.display(),
                trust_failure(trust.status),
                trust.status as u32
            )));
        }
        return signer_subject(&trust)
            .context("Unreadable catalog signer")
            .map(CatalogSignature::Valid);
    }
    Ok(CatalogSignature::Absent)
}

enum TrustSubject {
    File(WINTRUST_FILE_INFO),
    Catalog(WINTRUST_CATALOG_INFO),
}

/// One WinVerifyTrust VERIFY call; Drop issues the matching CLOSE, which also
/// releases provider data. Closing state VERIFY never populated is a no-op.
struct TrustState {
    data: WINTRUST_DATA,
    subject: TrustSubject,
    status: i32,
    last_error: u32,
}
impl TrustState {
    /// # Safety
    /// Every pointer and handle in `subject` must stay valid until the
    /// returned state drops.
    unsafe fn verify(subject: TrustSubject) -> Box<Self> {
        let mut state = Box::new(Self {
            // SAFETY: plain C struct for which all-zero is the documented initial value.
            data: unsafe { std::mem::zeroed() },
            subject,
            status: 0,
            last_error: 0,
        });
        // Boxed, so the union pointer into `subject` stays valid.
        let state_ref = &mut *state;
        state_ref.data.cbStruct = size_of::<WINTRUST_DATA>() as u32;
        state_ref.data.dwUIChoice = WTD_UI_NONE;
        state_ref.data.fdwRevocationChecks = WTD_REVOKE_NONE;
        match &mut state_ref.subject {
            TrustSubject::File(file) => {
                state_ref.data.dwUnionChoice = WTD_CHOICE_FILE;
                state_ref.data.Anonymous.pFile = file;
            }
            TrustSubject::Catalog(catalog) => {
                state_ref.data.dwUnionChoice = WTD_CHOICE_CATALOG;
                state_ref.data.Anonymous.pCatalog = catalog;
            }
        }
        state_ref.data.dwStateAction = WTD_STATEACTION_VERIFY;
        let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
        // SAFETY: fully initialised data; the caller keeps subject pointers alive.
        state_ref.status = unsafe {
            WinVerifyTrust(null_mut(), &mut action, (&mut state_ref.data as *mut WINTRUST_DATA).cast())
        };
        // SAFETY: read immediately after WinVerifyTrust, as its contract requires.
        state_ref.last_error = unsafe { GetLastError() };
        state
    }
}
impl Drop for TrustState {
    fn drop(&mut self) {
        self.data.dwStateAction = WTD_STATEACTION_CLOSE;
        let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
        // SAFETY: closes the state opened by the VERIFY call on this same data.
        unsafe {
            WinVerifyTrust(null_mut(), &mut action, (&mut self.data as *mut WINTRUST_DATA).cast());
        }
    }
}

fn signer_subject(trust: &TrustState) -> Result<Vec<u8>> {
    // SAFETY: the state data comes from a successful VERIFY that is still open;
    // every returned pointer is null-checked before it is read, and the subject
    // is copied out before the state closes.
    unsafe {
        let provider = WTHelperProvDataFromStateData(trust.data.hWVTStateData);
        ensure!(!provider.is_null(), "No Authenticode provider data");
        let signer = WTHelperGetProvSignerFromChain(provider, 0, 0, 0);
        ensure!(!signer.is_null(), "No Authenticode signer");
        let certificate = WTHelperGetProvCertFromChain(signer, 0);
        ensure!(!certificate.is_null(), "No Authenticode signer certificate");
        let context = (*certificate).pCert;
        ensure!(!context.is_null(), "No Authenticode signer certificate context");
        let info = (*context).pCertInfo;
        ensure!(!info.is_null(), "No Authenticode signer certificate info");
        let subject = (*info).Subject;
        let length = subject.cbData as usize;
        ensure!(
            !subject.pbData.is_null() && (1..=MAX_SUBJECT_DER).contains(&length),
            "Malformed Authenticode signer subject"
        );
        let subject = std::slice::from_raw_parts(subject.pbData, length).to_vec();
        validate_name_der(&subject)?;
        Ok(subject)
    }
}

/// Requires one definite-length, minimally encoded DER SEQUENCE spanning the
/// whole buffer with at least one RDN.
fn validate_name_der(der: &[u8]) -> Result<()> {
    const MALFORMED: &str = "Malformed Authenticode signer subject";
    let (&tag, rest) = der.split_first().context(MALFORMED)?;
    ensure!(tag == 0x30, MALFORMED);
    let (&first, rest) = rest.split_first().context(MALFORMED)?;
    let (length, rest) = match first {
        0..=0x7f => (first as usize, rest),
        0x81..=0x84 => {
            let count = (first & 0x7f) as usize;
            ensure!(rest.len() >= count && rest[0] != 0, MALFORMED);
            let length = rest[..count]
                .iter()
                .fold(0usize, |length, byte| (length << 8) | *byte as usize);
            ensure!(length >= 0x80, MALFORMED);
            (length, &rest[count..])
        }
        _ => bail!(MALFORMED),
    };
    ensure!(length > 0 && length == rest.len(), MALFORMED);
    Ok(())
}

struct CatalogAdmin(isize);
impl Drop for CatalogAdmin {
    fn drop(&mut self) {
        // SAFETY: releases the context acquired by CryptCATAdminAcquireContext2.
        unsafe { CryptCATAdminReleaseContext(self.0, 0) };
    }
}

/// Declared after its [`CatalogAdmin`] so it is released first.
struct CatalogContext {
    admin: isize,
    handle: isize,
}
impl Drop for CatalogContext {
    fn drop(&mut self) {
        // SAFETY: releases the context returned by CryptCATAdminEnumCatalogFromHash.
        unsafe { CryptCATAdminReleaseCatalogContext(self.admin, self.handle, 0) };
    }
}

fn trust_failure(status: i32) -> &'static str {
    match status {
        TRUST_E_BAD_DIGEST => "file contents do not match the signature",
        TRUST_E_MALFORMED_SIGNATURE | TRUST_E_CERT_SIGNATURE | TRUST_E_NO_SIGNER_CERT => {
            "signature is malformed"
        }
        CERT_E_UNTRUSTEDROOT | CERT_E_UNTRUSTEDTESTROOT | CERT_E_CHAINING => {
            "signer certificate is not trusted"
        }
        TRUST_E_EXPLICIT_DISTRUST | TRUST_E_SUBJECT_NOT_TRUSTED => "signer is explicitly distrusted",
        CERT_E_REVOKED | CRYPT_E_REVOKED => "signer certificate is revoked",
        CERT_E_EXPIRED | TRUST_E_TIME_STAMP => "signer certificate expired or timestamp is invalid",
        CRYPT_E_SECURITY_SETTINGS => "signature rejected by local security policy",
        TRUST_E_SUBJECT_FORM_UNKNOWN => "file format does not support Authenticode",
        CRYPT_E_FILE_ERROR => "file could not be read",
        _ => "Authenticode verification failed",
    }
}

/// Owned SID in DWORD-aligned storage.
struct Sid(Vec<u32>);
impl Sid {
    /// # Safety
    /// `sid` must be valid for `available` readable bytes.
    unsafe fn copy_from(sid: *const u8, available: usize) -> Result<Self> {
        // SAFETY: forwarded caller contract.
        let length = unsafe { sid_length(sid, available) }?;
        let mut owned = vec![0u32; length.div_ceil(size_of::<u32>())];
        // SAFETY: `owned` holds at least `length` bytes; the source is a valid SID.
        ensure!(
            unsafe {
                CopySid(
                    (owned.len() * size_of::<u32>()) as u32,
                    owned.as_mut_ptr().cast(),
                    sid as PSID,
                )
            } != 0,
            "CopySid failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(Self(owned))
    }
    fn as_psid(&self) -> PSID {
        // The Win32 APIs taking this pointer only read the SID.
        self.0.as_ptr() as PSID
    }
    fn byte_len(&self) -> usize {
        // SAFETY: holds a SID validated on construction.
        unsafe { GetLengthSid(self.as_psid()) as usize }
    }
}

/// Returns the length of the SID at `sid` after checking it fits `available`.
///
/// # Safety
/// `sid` must be valid for `available` readable bytes.
unsafe fn sid_length(sid: *const u8, available: usize) -> Result<usize> {
    const FIXED: usize = 8;
    ensure!(available >= FIXED, "Truncated SID");
    // SAFETY: byte 1 (SubAuthorityCount) lies within the checked bytes.
    let count = unsafe { ptr::read(sid.add(1)) } as usize;
    ensure!(count <= SID_MAX_SUB_AUTHORITIES, "Malformed SID");
    let length = FIXED + count * size_of::<u32>();
    ensure!(length <= available, "Truncated SID");
    // SAFETY: the whole SID lies within the checked bytes.
    ensure!(
        unsafe { IsValidSid(sid as PSID) } != 0 && unsafe { GetLengthSid(sid as PSID) } as usize == length,
        "Malformed SID"
    );
    Ok(length)
}

/// # Safety
/// `dacl` must be valid for `available` readable bytes.
unsafe fn check_dacl(dacl: *const ACL, available: usize, allowed: &[Sid]) -> Result<()> {
    ensure!(available >= size_of::<ACL>(), "Truncated DACL");
    // SAFETY: the ACL header lies within the checked bytes.
    let header = unsafe { ptr::read_unaligned(dacl) };
    let size = header.AclSize as usize;
    ensure!(size >= size_of::<ACL>() && size <= available, "Malformed DACL");
    let start = dacl as usize;
    let end = start + size;
    for index in 0..u32::from(header.AceCount) {
        let mut ace: *mut c_void = null_mut();
        // SAFETY: `dacl` is a bounded ACL and `index` is below its ACE count.
        ensure!(unsafe { GetAce(dacl, index, &mut ace) } != 0, "Malformed DACL entry");
        let ace_start = ace as usize;
        ensure!(
            ace_start >= start + size_of::<ACL>() && ace_start + size_of::<ACE_HEADER>() <= end,
            "DACL entry lies outside its ACL"
        );
        // SAFETY: the ACE header lies within the ACL.
        let ace_header = unsafe { ptr::read_unaligned(ace.cast::<ACE_HEADER>()) };
        let ace_size = ace_header.AceSize as usize;
        ensure!(
            ace_size >= ACCESS_ACE_SID_OFFSET && ace_start + ace_size <= end,
            "DACL entry lies outside its ACL"
        );
        let grants = match ace_header.AceType {
            ACCESS_ALLOWED_ACE_TYPE => true,
            ACCESS_DENIED_ACE_TYPE => false,
            other => bail!("Unsupported DACL entry type {other}"),
        };
        // SAFETY: the SID region lies within this ACE.
        let sid = unsafe { ace.cast::<u8>().add(ACCESS_ACE_SID_OFFSET) };
        // SAFETY: bounded by the ACE's remaining bytes.
        unsafe { sid_length(sid, ace_size - ACCESS_ACE_SID_OFFSET) }?;
        if grants {
            ensure!(
                allowed
                    .iter()
                    // SAFETY: both SIDs were validated.
                    .any(|allowed| unsafe { EqualSid(allowed.as_psid(), sid as PSID) } != 0),
                "DACL grants access to another account"
            );
        }
    }
    Ok(())
}

fn current_user_sid() -> Result<Sid> {
    let mut token: HANDLE = null_mut();
    // SAFETY: pseudo process handle and out-pointer.
    ensure!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } != 0,
        "OpenProcessToken failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: OpenProcessToken returned an owned token handle.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut length = 0u32;
    // SAFETY: size query with a null buffer.
    let queried = unsafe {
        GetTokenInformation(token.as_raw_handle() as HANDLE, TokenUser, null_mut(), 0, &mut length)
    };
    ensure!(
        // SAFETY: read immediately after the size query.
        queried == 0 && unsafe { GetLastError() } == ERROR_INSUFFICIENT_BUFFER,
        "GetTokenInformation size query failed: {}",
        std::io::Error::last_os_error()
    );
    ensure!(
        (size_of::<TOKEN_USER>() as u32..=MAX_TOKEN_USER).contains(&length),
        "Unexpected TOKEN_USER size {length}"
    );
    // Pointer-aligned storage for TOKEN_USER.
    let mut buffer = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
    let capacity = buffer.len() * size_of::<usize>();
    // SAFETY: `buffer` holds `capacity` writable, pointer-aligned bytes.
    ensure!(
        unsafe {
            GetTokenInformation(
                token.as_raw_handle() as HANDLE,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                capacity as u32,
                &mut length,
            )
        } != 0,
        "GetTokenInformation failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: the buffer is aligned for TOKEN_USER and at least that large.
    let user = unsafe { ptr::read(buffer.as_ptr().cast::<TOKEN_USER>()) };
    let start = buffer.as_ptr() as usize;
    let sid = user.User.Sid as usize;
    ensure!(
        sid >= start && sid < start + capacity,
        "Token user SID lies outside its buffer"
    );
    // SAFETY: the SID lies inside `buffer`, bounded by its end.
    unsafe { Sid::copy_from(sid as *const u8, start + capacity - sid) }
}

fn well_known_sid(kind: WELL_KNOWN_SID_TYPE) -> Result<Sid> {
    let mut storage = vec![0u32; SECURITY_MAX_SID_SIZE as usize / size_of::<u32>()];
    let mut length = SECURITY_MAX_SID_SIZE;
    // SAFETY: `storage` holds SECURITY_MAX_SID_SIZE writable bytes.
    ensure!(
        unsafe { CreateWellKnownSid(kind, null_mut(), storage.as_mut_ptr().cast(), &mut length) } != 0,
        "CreateWellKnownSid failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: CreateWellKnownSid wrote a SID into `storage`.
    unsafe { Sid::copy_from(storage.as_ptr().cast(), SECURITY_MAX_SID_SIZE as usize) }
}

struct LocalAllocation(*mut c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer was allocated by the API that returned it with LocalAlloc.
            unsafe { LocalFree(self.0) };
        }
    }
}

fn wide(path: &Path) -> Result<Vec<u16>> {
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    ensure!(!wide.contains(&0), "Path contains NUL: {}", path.display());
    wide.push(0);
    Ok(wide)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Security::{AddAccessAllowedAce, AddAccessDeniedAce, WinWorldSid};

    fn signed(subject: &[u8]) -> Authenticode {
        Authenticode::Signed {
            subject: subject.to_vec(),
        }
    }

    #[test]
    fn update_continuity_follows_publisher_and_unsigned_policy() {
        use Authenticode::Unsigned;
        use UnsignedPolicy::{AllowSignedUpgrade, PreserveUnsigned};
        let a = b"\x30\x03\x31\x01\x00";
        let b = b"\x30\x03\x31\x01\x01";
        for policy in [AllowSignedUpgrade, PreserveUnsigned] {
            assert!(update_continuity(&signed(a), &signed(a), policy).is_ok());
            assert!(update_continuity(&signed(a), &signed(b), policy).is_err());
            assert!(update_continuity(&signed(a), &Unsigned, policy).is_err());
            assert!(update_continuity(&Unsigned, &Unsigned, policy).is_ok());
        }
        assert!(update_continuity(&Unsigned, &signed(a), AllowSignedUpgrade).is_ok());
        assert!(update_continuity(&Unsigned, &signed(a), PreserveUnsigned).is_err());
    }

    #[test]
    fn signer_subject_must_be_one_complete_der_sequence() {
        assert!(validate_name_der(b"\x30\x03\x31\x01\x00").is_ok());
        let mut long = vec![0x30, 0x81, 0x80];
        long.extend([0u8; 0x80]);
        assert!(validate_name_der(&long).is_ok());
        let cases: [&[u8]; 7] = [
            b"",
            b"\x30\x00",
            b"\x31\x03\x31\x01\x00",
            b"\x30\x03\x31\x01",
            b"\x30\x03\x31\x01\x00\x00",
            b"\x30\x80\x31\x01\x00\x00\x00",
            b"\x30\x81\x03\x31\x01\x00",
        ];
        for malformed in cases {
            assert!(validate_name_der(malformed).is_err(), "{malformed:?}");
        }
    }

    /// Builds an ACL of (allow, sid) entries in DWORD-aligned storage.
    fn acl(entries: &[(bool, &Sid)]) -> Vec<u32> {
        let bytes = size_of::<ACL>()
            + entries
                .iter()
                .map(|(_, sid)| ACCESS_ACE_SID_OFFSET + sid.byte_len())
                .sum::<usize>();
        let mut storage = vec![0u32; bytes.div_ceil(4)];
        let acl = storage.as_mut_ptr().cast::<ACL>();
        unsafe {
            assert_ne!(InitializeAcl(acl, (storage.len() * 4) as u32, ACL_REVISION), 0);
            for (allow, sid) in entries {
                let added = if *allow {
                    AddAccessAllowedAce(acl, ACL_REVISION, FILE_ALL_ACCESS, sid.as_psid())
                } else {
                    AddAccessDeniedAce(acl, ACL_REVISION, FILE_ALL_ACCESS, sid.as_psid())
                };
                assert_ne!(added, 0);
            }
        }
        storage
    }

    fn check(entries: &[(bool, &Sid)], allowed: &[Sid]) -> Result<()> {
        let storage = acl(entries);
        unsafe { check_dacl(storage.as_ptr().cast(), storage.len() * 4, allowed) }
    }

    #[test]
    fn dacl_allows_only_listed_accounts_and_ignores_deny_entries() {
        let user = current_user_sid().unwrap();
        let system = well_known_sid(WinLocalSystemSid).unwrap();
        let everyone = well_known_sid(WinWorldSid).unwrap();
        let only_user = [current_user_sid().unwrap()];
        let user_and_system = [current_user_sid().unwrap(), well_known_sid(WinLocalSystemSid).unwrap()];

        assert!(check(&[(true, &user)], &only_user).is_ok());
        assert!(check(&[(true, &user), (false, &everyone)], &only_user).is_ok());
        assert!(check(&[(true, &user), (true, &system)], &only_user).is_err());
        assert!(check(&[(true, &user), (true, &system)], &user_and_system).is_ok());
        assert!(check(&[(true, &user), (true, &everyone)], &user_and_system).is_err());
    }

    #[test]
    fn restricted_directory_passes_until_another_account_is_granted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private");
        std::fs::create_dir(&path).unwrap();
        restrict_directory_to_current_user(&path).unwrap();
        verify_private_directory(&path, AllowedDirectoryUsers::CurrentUser).unwrap();
        verify_private_directory(&path, AllowedDirectoryUsers::CurrentUserAndSystem).unwrap();

        let user = current_user_sid().unwrap();
        let everyone = well_known_sid(WinWorldSid).unwrap();
        let widened = acl(&[(true, &user), (true, &everyone)]);
        let path_wide = wide(&path).unwrap();
        let status = unsafe {
            SetNamedSecurityInfoW(
                path_wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(),
                null_mut(),
                widened.as_ptr().cast::<ACL>(),
                null(),
            )
        };
        assert_eq!(status, ERROR_SUCCESS);
        assert!(verify_private_directory(&path, AllowedDirectoryUsers::CurrentUserAndSystem).is_err());
    }

    #[test]
    fn system_binary_verifies_through_its_signature() {
        // Inbox binaries are catalog-signed (or embedded-signed on some builds).
        let root = std::env::var_os("SystemRoot").unwrap();
        let notepad = Path::new(&root).join("System32").join("notepad.exe");
        assert!(matches!(authenticode(&notepad).unwrap(), Authenticode::Signed { .. }));
        verify_update_signature(&notepad, &notepad, UnsignedPolicy::PreserveUnsigned).unwrap();
    }

    #[test]
    fn unsigned_executable_is_accepted_as_unsigned() {
        // Test binaries are never Authenticode- or catalog-signed.
        let exe = std::env::current_exe().unwrap();
        assert_eq!(authenticode(&exe).unwrap(), Authenticode::Unsigned);
        verify_local_signature(&exe).unwrap();
        verify_update_signature(&exe, &exe, UnsignedPolicy::PreserveUnsigned).unwrap();
    }

    /// Minimal PE image: DOS header, signature, COFF header and an optional
    /// header of `optional_size` bytes declaring `count` data directories.
    fn pe_image(magic: u16, count: u32, optional_size: u16, security: (u32, u32)) -> Vec<u8> {
        let pe = 0x80usize;
        let (count_offset, directories) = if magic == 0x10b { (92, 96) } else { (108, 112) };
        let mut image = vec![0u8; pe + 24 + usize::from(optional_size)];
        image[..2].copy_from_slice(b"MZ");
        image[60..64].copy_from_slice(&(pe as u32).to_le_bytes());
        image[pe..pe + 4].copy_from_slice(b"PE\0\0");
        image[pe + 20..pe + 22].copy_from_slice(&optional_size.to_le_bytes());
        let optional = pe + 24;
        image[optional..optional + 2].copy_from_slice(&magic.to_le_bytes());
        image[optional + count_offset..optional + count_offset + 4].copy_from_slice(&count.to_le_bytes());
        let entry = optional + directories + 4 * 8;
        if entry + 8 <= image.len() {
            image[entry..entry + 4].copy_from_slice(&security.0.to_le_bytes());
            image[entry + 4..entry + 8].copy_from_slice(&security.1.to_le_bytes());
        }
        image
    }

    fn entry(image: Vec<u8>) -> Result<Option<(u32, u32)>> {
        certificate_table_entry(std::io::Cursor::new(image))
            .map(|entry| entry.map(|entry| (entry.address, entry.size)))
    }

    #[test]
    fn certificate_table_entry_reads_pe32_and_pe32_plus_headers() {
        for magic in [0x10b, 0x20b] {
            assert_eq!(entry(pe_image(magic, 16, 240, (0, 0))).unwrap(), Some((0, 0)));
            assert_eq!(entry(pe_image(magic, 16, 240, (0x400, 0x20))).unwrap(), Some((0x400, 0x20)));
            assert_eq!(entry(pe_image(magic, 4, 240, (0x400, 0x20))).unwrap(), None);
        }
    }

    #[test]
    fn certificate_table_entry_rejects_malformed_headers() {
        // Optional header too short to hold the security directory it counts.
        assert!(entry(pe_image(0x20b, 16, 112 + 4 * 8, (0, 0))).is_err());
        assert!(entry(pe_image(0x30b, 16, 240, (0, 0))).is_err());
        let mut not_pe = pe_image(0x20b, 16, 240, (0, 0));
        not_pe[0x80] = b'X';
        assert!(entry(not_pe).is_err());
        let mut truncated = pe_image(0x20b, 16, 240, (0, 0));
        truncated.truncate(0x80 + 24 + 100);
        assert!(entry(truncated).is_err());
        let mut truncated_optional = pe_image(0x20b, 4, 240, (0, 0));
        truncated_optional.truncate(0x80 + 24 + 112);
        assert!(entry(truncated_optional).is_err());
        assert!(entry(b"MZ".to_vec()).is_err());
    }

    /// Copies the unsigned test binary and points its certificate table at
    /// `address`/`size`, appending `table` at 8-byte alignment first.
    fn declared_table_fixture(dir: &Path, name: &str, table: &[u8], past_end: bool) -> std::path::PathBuf {
        let mut image = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let offset = certificate_table_entry(std::io::Cursor::new(&image))
            .unwrap()
            .expect("test binary declares a security directory slot")
            .offset as usize;
        image.resize(image.len().next_multiple_of(8), 0);
        let address = image.len() as u32 + if past_end { 0x1000 } else { 0 };
        image.extend_from_slice(table);
        image[offset..offset + 4].copy_from_slice(&address.to_le_bytes());
        image[offset + 4..offset + 8].copy_from_slice(&(table.len() as u32).to_le_bytes());
        let path = dir.join(name);
        std::fs::write(&path, image).unwrap();
        path
    }

    #[test]
    fn declared_but_invalid_certificate_table_is_not_unsigned() {
        let dir = tempfile::tempdir().unwrap();
        // WIN_CERTIFICATE: length 16, revision 2.0, PKCS_SIGNED_DATA, garbage body.
        let mut table = 16u32.to_le_bytes().to_vec();
        table.extend(0x0200u16.to_le_bytes());
        table.extend(0x0002u16.to_le_bytes());
        table.extend([0xab; 8]);
        for (name, past_end) in [("garbage.exe", false), ("dangling.exe", true)] {
            let fixture = declared_table_fixture(dir.path(), name, &table, past_end);
            assert!(authenticode(&fixture).is_err(), "{name}");
            assert!(verify_local_signature(&fixture).is_err(), "{name}");
            assert!(
                verify_update_signature(&fixture, &fixture, UnsignedPolicy::AllowSignedUpgrade).is_err(),
                "{name}"
            );
        }
    }
}

//! Stage complete standalone runtimes into immutable destinations.
//!
//! Explicit local operator builds and official releases converge here on one
//! validated, durable, content-identified tree. Activation, journaling and
//! rollback belong to the update operation engine, never to preparation.
use super::*;
use crate::daemon_admin::update_operation::RuntimeIdentity;
use sha2::Sha256;
use std::ffi::OsStr;

/// Complete runtime packaged from a local build (`cargo xtask stage-runtime
/// --kind standalone-local`). Never an official release.
pub(crate) const STANDALONE_LOCAL: &str = "standalone-local";
/// Official release archive verified against the configured repository.
pub(crate) const STANDALONE_ARCHIVE: &str = "standalone-archive";
const PROVENANCE: &str = "bundle-provenance.json";
const DESKTOP_DESCRIPTOR: &str = "desktop-runtime.json";
const NOTICES: [&str; 4] = [
    "LICENSE",
    "THIRD-PARTY-NOTICES.txt",
    "RUST-THIRD-PARTY-NOTICES.json",
    "RUST-THIRD-PARTY-NOTICES.txt",
];
const MAX_ENTRIES: usize = 100_000;
/// Free space kept beyond the candidate so staging never fills the volume.
const SPACE_HEADROOM: u64 = 256 * 1024 * 1024;
const DESKTOP_MANAGED: &str =
    "This runtime is managed by Paseo Desktop. Update Paseo Desktop on the host.";
const BARE_EXECUTABLE: &str = "A bare executable is not a complete runtime. Package the existing build with `cargo xtask stage-runtime DESTINATION --kind standalone-local --profile release` and pass DESTINATION";

/// Fingerprint one local update input at admission, before anything is staged.
///
/// Directory: `directory:sha512:<tree digest>` over the normalized modes
/// staging assigns, equal to the staged `RuntimeIdentity::tree_digest`.
/// Archive: `archive:<tar.gz|zip>:sha512:<digest of the exact bytes>`.
/// Unsafe inputs (symlinked source, desktop bundle, bare executable, links,
/// special files, non-portable names, oversize) are rejected here as well.
pub async fn input_digest(source: &Path) -> Result<String> {
    let source = source.to_owned();
    tokio::task::spawn_blocking(move || {
        let (source, format) = classify_source(&source)?;
        match format {
            None => {
                scan_directory(&source)?;
                Ok(directory_label(&digest_tree(&source, true)?))
            }
            Some(format) => {
                let mut input = open_regular(&source)?;
                let size = archive_size(&input)?;
                Ok(archive_label(
                    format,
                    &copy_hashed(&mut input, size, &mut std::io::sink())?,
                ))
            }
        }
    })
    .await?
}

/// Copy or extract one complete local standalone runtime into `destination`
/// exactly once, prove the staged bytes are the admitted input
/// (`expected_input_digest` from [`input_digest`]), then validate only the
/// frozen staged copy. The mutable source is never reread as proof.
///
/// `source` is a runtime directory or a `.tar.gz`/`.tgz`/`.zip` archive of
/// one. Same-semver candidates with different contents are accepted; only
/// `standalone-local` provenance (release=false) is admitted here, so a local
/// path can never claim official release provenance.
pub async fn prepare_local_pinned(
    source: &Path,
    destination: &Path,
    current_executable: &Path,
    expected_input_digest: &str,
) -> Result<RuntimeIdentity> {
    let current_root = runtime_root(current_executable)?;
    let (source, format) = classify_source(source)?;
    let stage = Stage::begin(destination, &[source.as_path(), current_root.as_path()])?;
    let (input, runtime, directory, pin) = (
        source.clone(),
        stage.runtime.clone(),
        stage.directory.path().to_owned(),
        expected_input_digest.to_owned(),
    );
    let staged_digest = tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        match format {
            None => {
                let (entries, size) = scan_directory(&input)?;
                ensure_space(&directory, size)?;
                copy_directory(&input, &runtime, &entries)?;
                seal(&runtime)?;
                // Sealed modes are the normalized modes `input_digest` hashed.
                let digest = tree_digest(&runtime)?;
                ensure_pinned(&pin, &directory_label(&digest))?;
                Ok(Some(digest))
            }
            Some(format) => {
                let frozen = directory.join(format!("input.{format}"));
                let digest = freeze_archive(&input, &frozen)?;
                ensure_pinned(&pin, &archive_label(format, &digest))?;
                ensure_space(&directory, expanded_size(&frozen, format)?)?;
                extract(&frozen, &runtime, format)?;
                fs::remove_file(&frozen)?;
                seal(&runtime)?;
                Ok(None)
            }
        }
    })
    .await??;
    let identity = examine(&stage.runtime, Some(STANDALONE_LOCAL), staged_digest).await?;
    validate_local_signature(&stage.runtime.join(executable_name(&identity.platform))).await?;
    stage.publish(identity).await
}

fn directory_label(tree_digest: &str) -> String {
    format!("directory:sha512:{tree_digest}")
}

fn archive_label(format: &str, digest: &str) -> String {
    format!("archive:{format}:sha512:{digest}")
}

fn ensure_pinned(expected: &str, staged: &str) -> Result<()> {
    if expected != staged {
        bail!("Local runtime input changed since the update was admitted; submit a new update")
    }
    Ok(())
}

/// Classify a local input without following a symlinked source path.
/// `None` is a runtime directory; `Some(format)` a runtime archive.
fn classify_source(source: &Path) -> Result<(PathBuf, Option<&'static str>)> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("Cannot read local runtime {}", source.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("Local runtime source must not be a symlink")
    }
    let source = source.canonicalize()?;
    if is_desktop_path(&source) {
        bail!(DESKTOP_MANAGED)
    }
    let format = if metadata.is_dir() {
        None
    } else if metadata.is_file() {
        Some(local_archive_format(&source)?)
    } else {
        bail!("Local runtime source must be a directory or runtime archive")
    };
    Ok((source, format))
}

/// Download, verify and stage the newest official release into `destination`.
///
/// Repository, tag, source commit, manifest/asset digest, signature continuity
/// and strictly-newer semver rules are unchanged. The installed runtime must
/// itself be a verified standalone runtime; a local standalone may move to a
/// genuine newer release without its own provenance being reinterpreted.
pub async fn prepare_release(
    current_executable: &Path,
    destination: &Path,
) -> Result<RuntimeIdentity> {
    let current_executable = current_executable.canonicalize()?;
    let current_root = runtime_root(&current_executable)?;
    let (platform, _) = platform()?;
    if current_executable.file_name() != Some(OsStr::new(executable_name(platform))) {
        bail!("Installed executable is not the standalone native runtime entry")
    }
    let current = inspect_runtime(&current_root).await.context(
        "The installed runtime is not a verified standalone runtime; update it from a complete local runtime first",
    )?;
    let stage = Stage::begin(destination, &[current_root.as_path()])?;
    let client = http()?;
    let release = discover(&client).await?;
    if semver::Version::parse(&release.version)? <= semver::Version::parse(&current.version)? {
        bail!("No newer native daemon release is available")
    }
    let directory = stage.directory.path().to_owned();
    ensure_space(&directory, release.size)?;
    let archive = directory.join(&release.name);
    download(&client, &release, &archive).await?;
    let (runtime, format) = (stage.runtime.clone(), release.format.clone());
    tokio::task::spawn_blocking(move || {
        ensure_space(&directory, expanded_size(&archive, &format)?)?;
        extract(&archive, &runtime, &format)?;
        fs::remove_file(&archive)?;
        seal(&runtime)
    })
    .await??;
    let identity = examine(&stage.runtime, Some(STANDALONE_ARCHIVE), None).await?;
    validate_marker(
        &read_marker(&stage.runtime)?,
        &release.version,
        &release.source_sha,
        &release.platform,
        &release.arch,
    )?;
    validate_signature(
        &current_executable,
        &stage.runtime.join(executable_name(platform)),
    )
    .await?;
    stage.publish(identity).await
}

/// Strictly validate a complete `standalone-local` or `standalone-archive`
/// runtime and return its identity. Desktop bundles, legacy markers and
/// incomplete or inconsistent trees are rejected rather than reinterpreted.
pub async fn inspect_runtime(root: &Path) -> Result<RuntimeIdentity> {
    if fs::symlink_metadata(root)?.file_type().is_symlink() {
        bail!("Runtime root cannot be a symlink")
    }
    examine(&root.canonicalize()?, None, None).await
}

/// Recheck a prepared runtime: same canonical root, provenance, executable
/// image and complete tree digest as when it was staged.
pub async fn verify_runtime(identity: &RuntimeIdentity) -> Result<()> {
    let observed = inspect_runtime(&identity.root)
        .await
        .with_context(|| format!("Runtime {} is no longer valid", identity.root.display()))?;
    if observed.root != identity.root
        || observed.version != identity.version
        || observed.source_sha != identity.source_sha
        || observed.platform != identity.platform
        || observed.arch != identity.arch
        || observed.release != identity.release
        || observed.executable_sha256 != identity.executable_sha256
        || observed.tree_digest != identity.tree_digest
    {
        bail!(
            "Native runtime changed after preparation: {}",
            identity.root.display()
        )
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hex::encode(hash.finalize()))
}

fn runtime_root(executable: &Path) -> Result<PathBuf> {
    Ok(executable
        .canonicalize()?
        .parent()
        .context("Executable has no runtime root")?
        .to_owned())
}

fn is_desktop_path(path: &Path) -> bool {
    path.components().any(|c| {
        let name = c.as_os_str().to_string_lossy().to_ascii_lowercase();
        name.ends_with(".app") || name.ends_with(".appimage")
    })
}

fn valid_hex(value: &str, lengths: &[usize]) -> bool {
    lengths.contains(&value.len()) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A staging directory beside the absent destination, published by an
/// atomic no-replace rename only after the whole tree is validated and durable.
struct Stage {
    directory: tempfile::TempDir,
    runtime: PathBuf,
    destination: PathBuf,
}

impl Stage {
    fn begin(destination: &Path, disjoint: &[&Path]) -> Result<Self> {
        if !destination.is_absolute() {
            bail!("Runtime destination must be an absolute path")
        }
        let name = match destination.components().next_back() {
            Some(Component::Normal(name)) => name.to_owned(),
            _ => bail!("Invalid runtime destination"),
        };
        safe_member(Path::new(&name))?;
        match fs::symlink_metadata(destination) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => bail!(
                "Runtime destination already exists: {}",
                destination.display()
            ),
            Err(e) => return Err(e.into()),
        }
        let parent = destination
            .parent()
            .context("Runtime destination has no parent")?;
        let metadata = fs::symlink_metadata(parent)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            bail!("Runtime destination parent must be a real directory")
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.uid() != nix::unistd::geteuid().as_raw() || metadata.mode() & 0o022 != 0 {
                bail!("Runtime destination parent has unsafe ownership or permissions")
            }
        }
        let destination = parent.canonicalize()?.join(name);
        for other in disjoint {
            let other = other.canonicalize()?;
            if destination.starts_with(&other) || other.starts_with(&destination) {
                bail!(
                    "Runtime destination overlaps {}; stage into a separate immutable directory",
                    other.display()
                )
            }
        }
        let directory = tempfile::Builder::new()
            .prefix(".paseo-stage-")
            .tempdir_in(destination.parent().context("Missing parent")?)?;
        Ok(Self {
            runtime: directory.path().join("runtime"),
            directory,
            destination,
        })
    }

    /// The staged tree is already sealed; publish it without replacing anything.
    async fn publish(self, mut identity: RuntimeIdentity) -> Result<RuntimeIdentity> {
        let (from, to) = (self.runtime.clone(), self.destination.clone());
        tokio::task::spawn_blocking(move || rename_new(&from, &to)).await??;
        sync_parent(&self.destination).await?;
        identity.root = self.destination;
        Ok(identity)
    }
}

async fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let parent = path.parent().context("Missing runtime parent")?.to_owned();
        tokio::task::spawn_blocking(move || File::open(parent)?.sync_all()).await??;
    }
    Ok(())
}

fn rename_new(from: &Path, to: &Path) -> Result<()> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let source = std::ffi::CString::new(from.as_os_str().as_bytes())?;
        let target = std::ffi::CString::new(to.as_os_str().as_bytes())?;
        // SAFETY: both paths are valid NUL-terminated strings for this call.
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                target.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        // Filesystems without RENAME_NOREPLACE fall through to the checked rename.
        if !matches!(error.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS)) {
            return Err(error).context("Cannot publish staged runtime");
        }
    }
    if fs::symlink_metadata(to).is_ok() {
        bail!("Runtime destination appeared during staging: {}", to.display())
    }
    fs::rename(from, to).context("Cannot publish staged runtime")
}

fn local_archive_format(path: &Path) -> Result<&'static str> {
    let name = path
        .file_name()
        .context("Invalid local runtime path")?
        .to_string_lossy()
        .to_ascii_lowercase();
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        return Ok("tar.gz");
    }
    if name.ends_with(".zip") {
        return Ok("zip");
    }
    const MACH_O: [[u8; 4]; 3] = [
        [0xcf, 0xfa, 0xed, 0xfe],
        [0xca, 0xfe, 0xba, 0xbe],
        [0xfe, 0xed, 0xfa, 0xcf],
    ];
    let mut magic = [0u8; 4];
    let count = File::open(path)?.read(&mut magic)?;
    let magic = &magic[..count];
    if magic.starts_with(b"\x7fELF")
        || magic.starts_with(b"MZ")
        || magic.starts_with(b"#!")
        || MACH_O.iter().any(|m| magic == m.as_slice())
    {
        bail!(BARE_EXECUTABLE)
    }
    bail!("Unsupported local runtime file; pass a runtime directory or a .tar.gz/.zip of one")
}

struct SourceEntry {
    relative: PathBuf,
    directory: bool,
    size: u64,
    mode: u32,
}

/// Inventory a source runtime without following links. Only regular files and
/// directories with portable names are admitted, bounded in count and size.
fn scan_directory(source: &Path) -> Result<(Vec<SourceEntry>, u64)> {
    let mut entries = Vec::new();
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(source)
        .follow_links(false)
        .sort_by_file_name()
        .min_depth(1)
    {
        let entry = entry?;
        let relative = safe_member(entry.path().strip_prefix(source)?)?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() || !(metadata.is_file() || metadata.is_dir()) {
            bail!(
                "Local runtime contains a symlink or special file: {}",
                relative.display()
            )
        }
        if entries.len() >= MAX_ENTRIES {
            bail!("Local runtime has too many entries")
        }
        let size = if metadata.is_file() { metadata.len() } else { 0 };
        total = total.checked_add(size).context("Runtime size overflow")?;
        if total > MAX_EXPANDED {
            bail!("Local runtime exceeds size limit")
        }
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode()
        };
        #[cfg(not(unix))]
        let mode = 0o644;
        entries.push(SourceEntry {
            relative,
            directory: metadata.is_dir(),
            size,
            mode,
        });
    }
    Ok((entries, total))
}

fn copy_directory(source: &Path, root: &Path, entries: &[SourceEntry]) -> Result<()> {
    fs::create_dir(root)?;
    for entry in entries {
        if entry.directory {
            fs::create_dir_all(root.join(&entry.relative))?;
            continue;
        }
        let mut file = open_regular(&source.join(&entry.relative))?;
        let metadata = file.metadata()?;
        if metadata.len() != entry.size {
            bail!(
                "Local runtime changed while staging: {}",
                entry.relative.display()
            )
        }
        write_member(root, &entry.relative, &mut file, entry.size, entry.mode)?;
        // Exactly the executable classification `input_digest` normalized,
        // independent of the staging process umask.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if entry.mode & 0o111 != 0 { 0o755 } else { 0o644 };
            fs::set_permissions(
                root.join(&entry.relative),
                fs::Permissions::from_mode(mode),
            )?;
        }
    }
    Ok(())
}

/// Open a regular file without following a final-component symlink.
fn open_regular(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        bail!("Local runtime input is not a regular file: {}", path.display())
    }
    Ok(file)
}

fn archive_size(input: &File) -> Result<u64> {
    let size = input.metadata()?.len();
    if size == 0 || size > MAX_ARCHIVE {
        bail!("Local runtime archive size is invalid")
    }
    Ok(size)
}

/// Stream exactly `size` bytes from `input` into `output`, returning their
/// SHA512 (base64). A file that grew or shrank meanwhile is rejected.
fn copy_hashed(input: &mut File, size: u64, output: &mut impl Write) -> Result<String> {
    let mut hash = Sha512::new();
    let mut buffer = [0u8; 65536];
    let mut copied = 0u64;
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        copied = copied
            .checked_add(count as u64)
            .context("Archive size overflow")?;
        if copied > size {
            break;
        }
        hash.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
    }
    if copied != size {
        bail!("Local runtime archive changed while it was read")
    }
    Ok(STANDARD.encode(hash.finalize()))
}

/// Snapshot a local archive once, hashing exactly the frozen bytes, so the
/// admission pin and all later validation never reread the source path.
fn freeze_archive(source: &Path, frozen: &Path) -> Result<String> {
    let mut input = open_regular(source)?;
    let size = archive_size(&input)?;
    ensure_space(frozen.parent().context("Missing stage")?, size)?;
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(frozen)?;
    let digest = copy_hashed(&mut input, size, &mut output)?;
    output.sync_all()?;
    Ok(digest)
}

/// Declared expanded payload size, checked before anything is extracted.
fn expanded_size(archive: &Path, format: &str) -> Result<u64> {
    let mut total = 0u64;
    let mut add = |size: u64| -> Result<()> {
        total = total.checked_add(size).context("Archive size overflow")?;
        if total > MAX_EXPANDED {
            bail!("Expanded runtime exceeds size limit")
        }
        Ok(())
    };
    match format {
        "tar.gz" => {
            let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(File::open(archive)?));
            for (count, entry) in archive.entries()?.enumerate() {
                if count >= MAX_ENTRIES {
                    bail!("Excessive runtime archive members")
                }
                add(entry?.size())?;
            }
        }
        "zip" => {
            let mut archive = zip::ZipArchive::new(File::open(archive)?)?;
            if archive.len() > MAX_ENTRIES {
                bail!("Excessive runtime archive members")
            }
            for index in 0..archive.len() {
                add(archive.by_index_raw(index)?.size())?;
            }
        }
        _ => bail!("Unsupported native runtime archive format"),
    }
    Ok(total)
}

fn ensure_space(directory: &Path, needed: u64) -> Result<()> {
    let required = needed
        .checked_add(SPACE_HEADROOM)
        .context("Runtime size overflow")?;
    let available = available_space(directory)?;
    if available < required {
        bail!(
            "Insufficient free space to stage the runtime in {}: {required} bytes required, {available} available",
            directory.display()
        )
    }
    Ok(())
}

#[cfg(unix)]
#[allow(clippy::useless_conversion)]
fn available_space(directory: &Path) -> Result<u64> {
    let stat = nix::sys::statvfs::statvfs(directory)?;
    Ok(u64::from(stat.blocks_available()).saturating_mul(u64::from(stat.fragment_size())))
}

#[cfg(windows)]
fn available_space(directory: &Path) -> Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    let wide = directory
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut available = 0u64;
    // SAFETY: `wide` is NUL-terminated and the out-pointer is a valid u64.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(available)
}

#[cfg(not(any(unix, windows)))]
fn available_space(_: &Path) -> Result<u64> {
    bail!("Native updates are unsupported on this platform")
}

/// Normalize modes independent of umask, then make file metadata and every
/// directory entry durable so the whole stage survives power loss.
fn seal(root: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .contents_first(true)
    {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() || !(metadata.is_file() || metadata.is_dir()) {
            bail!(
                "Runtime contains a symlink or special file: {}",
                entry.path().display()
            )
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let executable = metadata.is_dir() || metadata.permissions().mode() & 0o111 != 0;
            fs::set_permissions(
                entry.path(),
                fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
            )?;
            File::open(entry.path())?.sync_all()?;
        }
    }
    Ok(())
}

fn read_identity_file(root: &Path, name: &str) -> Result<Value> {
    let path = root.join(name);
    let metadata =
        fs::symlink_metadata(&path).with_context(|| format!("Runtime is missing {name}"))?;
    if !metadata.is_file() || metadata.len() > 64 * 1024 {
        bail!("Invalid runtime identity file {name}")
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

struct Claimed {
    version: String,
    source_sha: String,
    platform: String,
    arch: String,
    release: bool,
    executable_sha256: String,
}

/// Marker, bundle provenance, executable image and complete assets must agree.
fn claimed_identity(root: &Path, expected: Option<&str>) -> Result<Claimed> {
    let (platform, arch) = platform()?;
    if fs::symlink_metadata(root.join(DESKTOP_DESCRIPTOR)).is_ok() {
        bail!(DESKTOP_MANAGED)
    }
    let marker = read_identity_file(root, MARKER)?;
    let kind = marker["installationKind"].as_str().unwrap_or_default();
    match kind {
        "desktop-bundled" => bail!(DESKTOP_MANAGED),
        STANDALONE_LOCAL | STANDALONE_ARCHIVE => {}
        other => bail!(
            "Runtime installation kind {other:?} is not a standalone runtime; update it through its package manager"
        ),
    }
    if let Some(expected) = expected
        && expected != kind
    {
        bail!("Runtime is {kind}, but this update source requires {expected}")
    }
    let release = kind == STANDALONE_ARCHIVE;
    let version = marker["version"]
        .as_str()
        .context("Runtime marker has no version")?;
    let parsed = semver::Version::parse(version)?;
    let source = marker["sourceSha"]
        .as_str()
        .filter(|s| valid_hex(s, &[40, 64]))
        .context("Runtime marker has invalid source provenance")?;
    if marker["schemaVersion"] != 1
        || marker["runtime"] != "rust"
        || marker["appId"] != "sh.paseo.daemon"
        || marker["platform"] != platform
        || marker["arch"] != arch
        || marker["release"] != release
    {
        bail!("Runtime marker identity does not match this host or installation kind")
    }
    let executable = root.join(executable_name(platform));
    let metadata = fs::symlink_metadata(&executable).context("Runtime executable is missing")?;
    if !metadata.is_file() {
        bail!("Runtime executable is not a regular file")
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o100 == 0 {
            bail!("Runtime executable is not executable")
        }
    }
    let executable_sha256 = sha256_file(&executable)?;
    let provenance = read_identity_file(root, PROVENANCE)?;
    if provenance["schemaVersion"] != 1
        || provenance["runtime"] != "rust"
        || provenance["installationKind"] != kind
        || ["version", "sourceSha", "platform", "arch", "release"]
            .iter()
            .any(|key| provenance[key] != marker[key])
        || provenance["executableSha256"] != executable_sha256.as_str()
        || !provenance["sourceExecutableSha256"]
            .as_str()
            .is_some_and(|s| valid_hex(s, &[64]))
    {
        bail!("Runtime bundle provenance does not match its marker or executable image")
    }
    if release {
        let channel = provenance["channel"].as_str();
        if provenance["profile"] != "release"
            || provenance["repository"] != repository()?
            || !matches!(channel, Some("stable" | "beta"))
            || (channel == Some("stable") && !parsed.pre.is_empty())
        {
            bail!("Official runtime provenance is not a configured release")
        }
    } else if !matches!(provenance["profile"].as_str(), Some("release" | "debug"))
        || !provenance["channel"].is_null()
        || !provenance["repository"].is_null()
        || !matches!(provenance["sourceDirty"], Value::Bool(_) | Value::Null)
    {
        bail!("Local runtime provenance must describe a local build without release identity")
    }
    ensure_complete(root, platform)?;
    #[cfg(unix)]
    ensure_owned_tree(root)?;
    Ok(Claimed {
        version: version.into(),
        source_sha: source.to_ascii_lowercase(),
        platform: platform.into(),
        arch: arch.into(),
        release,
        executable_sha256,
    })
}

/// Every normal runtime asset produced by `bundle_runtime` must be present.
fn ensure_complete(root: &Path, platform: &str) -> Result<()> {
    let directories: &[&str] = if platform == "linux" {
        &["skills", "lib"]
    } else {
        &["skills"]
    };
    for name in directories {
        let metadata = fs::symlink_metadata(root.join(name))
            .with_context(|| format!("Runtime is missing {name}/"))?;
        if !metadata.is_dir() {
            bail!("Runtime {name} is not a directory")
        }
    }
    if fs::read_dir(root.join("skills"))?.next().is_none() {
        bail!("Runtime skills/ is empty")
    }
    for name in NOTICES {
        let metadata =
            fs::symlink_metadata(root.join(name)).with_context(|| format!("Runtime is missing {name}"))?;
        if !metadata.is_file() || metadata.len() == 0 {
            bail!("Runtime notice {name} is invalid")
        }
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_owned_tree(root: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let uid = nix::unistd::geteuid().as_raw();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
            bail!(
                "Runtime has unsafe ownership or permissions: {}",
                entry.path().display()
            )
        }
    }
    Ok(())
}

/// `staged_digest` is the tree digest already computed over this private,
/// sealed stage (pin check), reused instead of hashing the tree twice.
async fn examine(
    root: &Path,
    expected: Option<&'static str>,
    staged_digest: Option<String>,
) -> Result<RuntimeIdentity> {
    let path = root.to_owned();
    let claimed =
        tokio::task::spawn_blocking(move || claimed_identity(&path, expected)).await??;
    validate_binary(&root.join(executable_name(&claimed.platform))).await?;
    let tree_digest = match staged_digest {
        Some(digest) => digest,
        None => tree_digest_async(root.to_owned()).await?,
    };
    Ok(RuntimeIdentity {
        root: root.to_owned(),
        version: claimed.version,
        source_sha: claimed.source_sha,
        platform: claimed.platform,
        arch: claimed.arch,
        release: claimed.release,
        executable_sha256: claimed.executable_sha256,
        tree_digest,
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::{
        fs::{self, File},
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
    };

    fn elf(suffix: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0u8; 128];
        bytes[..6].copy_from_slice(b"\x7fELF\x02\x01");
        let machine: u16 = if platform().unwrap().1 == "x64" { 62 } else { 183 };
        bytes[18..20].copy_from_slice(&machine.to_le_bytes());
        bytes.extend_from_slice(suffix);
        bytes
    }

    fn write(path: &Path, bytes: &[u8], mode: u32) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn edit_identity(root: &Path, key: &str, value: Value) {
        for file in [MARKER, PROVENANCE] {
            let mut identity: Value =
                serde_json::from_slice(&fs::read(root.join(file)).unwrap()).unwrap();
            identity[key] = value.clone();
            write(&root.join(file), identity.to_string().as_bytes(), 0o644);
        }
    }

    /// Mirrors `cargo xtask stage-runtime --kind standalone-local` output.
    fn local_runtime(root: &Path, version: &str, binary: &[u8]) {
        let (platform, arch) = platform().unwrap();
        let source = "a".repeat(40);
        write(&root.join("paseo"), binary, 0o755);
        write(&root.join("skills/paseo/SKILL.md"), b"skill", 0o644);
        write(&root.join("lib/licenses/system/notice"), b"notice", 0o644);
        for name in NOTICES {
            write(&root.join(name), b"notice", 0o644);
        }
        let marker = json!({"schemaVersion":1,"runtime":"rust","appId":"sh.paseo.daemon","installationKind":STANDALONE_LOCAL,"version":version,"sourceSha":source,"platform":platform,"arch":arch,"release":false});
        write(&root.join(MARKER), marker.to_string().as_bytes(), 0o644);
        let executable = sha256_file(&root.join("paseo")).unwrap();
        let provenance = json!({"schemaVersion":1,"runtime":"rust","installationKind":STANDALONE_LOCAL,"version":version,"sourceSha":source,"platform":platform,"arch":arch,"release":false,"channel":null,"repository":null,"profile":"release","sourceDirty":true,"executableSha256":executable,"sourceExecutableSha256":executable});
        write(&root.join(PROVENANCE), provenance.to_string().as_bytes(), 0o644);
        // Independent of the test process umask.
        for entry in walkdir::WalkDir::new(root) {
            let entry = entry.unwrap();
            if entry.file_type().is_dir() {
                fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
    }

    /// Returns (installed executable, private runtimes directory).
    fn installation(temp: &Path) -> (PathBuf, PathBuf) {
        let root = temp.join("current");
        local_runtime(&root, "0.13.1", &elf(b"installed"));
        let runtimes = temp.join("runtimes");
        fs::create_dir(&runtimes).unwrap();
        fs::set_permissions(&runtimes, fs::Permissions::from_mode(0o700)).unwrap();
        (root.join("paseo"), runtimes)
    }

    /// Admission then staging, as the update operation performs them.
    async fn prepare_local(
        source: &Path,
        destination: &Path,
        executable: &Path,
    ) -> Result<RuntimeIdentity> {
        let pin = input_digest(source).await?;
        prepare_local_pinned(source, destination, executable, &pin).await
    }

    fn tar_gz(source: &Path, archive: &Path) {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
            File::create(archive).unwrap(),
            flate2::Compression::default(),
        ));
        tar.append_dir_all(".", source).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
    }

    #[tokio::test]
    async fn same_version_local_build_is_staged_frozen_and_rechecked() {
        let temp = tempfile::tempdir().unwrap();
        let (executable, runtimes) = installation(temp.path());
        let source = temp.path().join("candidate");
        local_runtime(&source, "0.13.1", &elf(b"rebuilt"));
        let destination = runtimes.join("candidate");
        let identity = prepare_local(&source, &destination, &executable)
            .await
            .unwrap();
        assert_eq!(identity.root, destination.canonicalize().unwrap());
        assert_eq!(identity.version, "0.13.1");
        assert!(!identity.release);
        let installed = inspect_runtime(executable.parent().unwrap()).await.unwrap();
        assert_eq!(installed.version, identity.version);
        assert_ne!(identity.executable_sha256, installed.executable_sha256);
        assert_ne!(identity.tree_digest, installed.tree_digest);
        // Source edits after staging never reach the frozen candidate.
        fs::write(source.join("skills/paseo/SKILL.md"), b"later edit").unwrap();
        verify_runtime(&identity).await.unwrap();
        assert_eq!(
            fs::read(destination.join("skills/paseo/SKILL.md")).unwrap(),
            b"skill"
        );
        let leftovers = fs::read_dir(&runtimes)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".paseo-stage-")
            })
            .count();
        assert_eq!(leftovers, 0);
        fs::write(destination.join("skills/paseo/SKILL.md"), b"tampered").unwrap();
        assert!(verify_runtime(&identity).await.is_err());
    }

    #[tokio::test]
    async fn local_archive_is_frozen_then_extracted() {
        let temp = tempfile::tempdir().unwrap();
        let (executable, runtimes) = installation(temp.path());
        let source = temp.path().join("candidate");
        local_runtime(&source, "0.13.2", &elf(b"archived"));
        let archive = temp.path().join("candidate.tar.gz");
        tar_gz(&source, &archive);
        let identity = prepare_local(&archive, &runtimes.join("candidate"), &executable)
            .await
            .unwrap();
        assert_eq!(identity.version, "0.13.2");
        verify_runtime(&identity).await.unwrap();
    }

    #[tokio::test]
    async fn admitted_input_digest_binds_the_staged_copy() {
        let temp = tempfile::tempdir().unwrap();
        let (executable, runtimes) = installation(temp.path());
        // Private source modes must not falsely mismatch the 0755/0644 stage.
        let private = temp.path().join("private");
        local_runtime(&private, "0.13.1", &elf(b"private"));
        for entry in walkdir::WalkDir::new(&private) {
            let entry = entry.unwrap();
            let executable = entry.file_type().is_dir() || entry.path().ends_with("paseo");
            let mode = if executable { 0o700 } else { 0o600 };
            fs::set_permissions(entry.path(), fs::Permissions::from_mode(mode)).unwrap();
        }
        let pin = input_digest(&private).await.unwrap();
        assert!(pin.starts_with("directory:sha512:"), "{pin}");
        let identity =
            prepare_local_pinned(&private, &runtimes.join("private"), &executable, &pin)
                .await
                .unwrap();
        assert_eq!(pin, directory_label(&identity.tree_digest));

        // A directory modified between admission and staging is refused.
        let changed = temp.path().join("changed");
        local_runtime(&changed, "0.13.1", &elf(b"changed"));
        let pin = input_digest(&changed).await.unwrap();
        fs::write(changed.join("skills/paseo/SKILL.md"), b"edited").unwrap();
        let destination = runtimes.join("changed");
        let error = prepare_local_pinned(&changed, &destination, &executable, &pin)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("changed since"), "{error}");
        assert!(!destination.exists());

        // An archive replaced between admission and staging is refused.
        let archive = temp.path().join("changed.tar.gz");
        tar_gz(&private, &archive);
        let pin = input_digest(&archive).await.unwrap();
        assert!(pin.starts_with("archive:tar.gz:sha512:"), "{pin}");
        tar_gz(&changed, &archive);
        let destination = runtimes.join("archive");
        let error = prepare_local_pinned(&archive, &destination, &executable, &pin)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("changed since"), "{error}");
        assert!(!destination.exists());
    }

    #[tokio::test]
    async fn local_preparation_rejects_incomplete_unsafe_or_misrepresented_runtimes() {
        let temp = tempfile::tempdir().unwrap();
        let (executable, runtimes) = installation(temp.path());
        let cases: [(&str, fn(&Path)); 8] = [
            ("symlink", |root| {
                std::os::unix::fs::symlink("/etc/passwd", root.join("skills/link")).unwrap()
            }),
            ("missing notice", |root| {
                fs::remove_file(root.join("LICENSE")).unwrap()
            }),
            ("missing libraries", |root| {
                fs::remove_dir_all(root.join("lib")).unwrap()
            }),
            ("desktop bundle", |root| {
                write(&root.join(DESKTOP_DESCRIPTOR), b"{}", 0o644)
            }),
            ("executable changed after packaging", |root| {
                write(&root.join("paseo"), &elf(b"swapped"), 0o755)
            }),
            ("claims official release", |root| {
                edit_identity(root, "release", json!(true))
            }),
            ("claims official archive", |root| {
                edit_identity(root, "installationKind", json!(STANDALONE_ARCHIVE))
            }),
            ("legacy desktop marker", |root| {
                edit_identity(root, "installationKind", json!("desktop-bundled"))
            }),
        ];
        for (index, (name, edit)) in cases.iter().enumerate() {
            let source = temp.path().join(format!("bad-{index}"));
            local_runtime(&source, "0.13.1", &elf(b"candidate"));
            edit(&source);
            let destination = runtimes.join(format!("bad-{index}"));
            assert!(
                prepare_local(&source, &destination, &executable)
                    .await
                    .is_err(),
                "{name}"
            );
            assert!(!destination.exists(), "{name}");
        }
        let bare = temp.path().join("paseo-bare");
        write(&bare, &elf(b"bare"), 0o755);
        let error = prepare_local(&bare, &runtimes.join("bare"), &executable)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("bare executable"), "{error}");
        let good = temp.path().join("good");
        local_runtime(&good, "0.13.1", &elf(b"good"));
        // Destination overlapping the source or installed runtime, or already present.
        assert!(
            prepare_local(&good, &good.join("staged"), &executable)
                .await
                .is_err()
        );
        assert!(
            prepare_local(&good, &executable.parent().unwrap().join("nested"), &executable)
                .await
                .is_err()
        );
        let existing = runtimes.join("existing");
        fs::create_dir(&existing).unwrap();
        assert!(
            prepare_local(&good, &existing, &executable)
                .await
                .is_err()
        );
        assert_eq!(fs::read_dir(&existing).unwrap().count(), 0);
    }
}

//! A cache entry attests that this exact inventory was fully verified for the target.
use super::*;

const DIRECTORY: &str = "native-update-cache";
const FILE: &str = "verified-releases-v1.json";
const MAX_ENTRIES: usize = 4;
const MAX_BYTES: u64 = 16 * 1024;

#[derive(PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Key {
    repository: String,
    channel: String,
    target: String,
    tag: String,
    pub(super) manifest_sha512: String,
}

impl Key {
    pub fn new(repository: &str, channel: &str, target: &str, tag: &str, bytes: &[u8]) -> Self {
        Self::with_digest(
            repository,
            channel,
            target,
            tag,
            STANDARD.encode(Sha512::digest(bytes)),
        )
    }

    pub fn with_digest(
        repository: &str,
        channel: &str,
        target: &str,
        tag: &str,
        manifest_sha512: String,
    ) -> Self {
        Self {
            repository: repository.into(),
            channel: channel.into(),
            target: target.into(),
            tag: tag.into(),
            manifest_sha512,
        }
    }
}

pub(super) async fn entries(home: &Path) -> Vec<Key> {
    let directory = home.join(DIRECTORY);
    tokio::task::spawn_blocking(move || read(&directory).unwrap_or_default())
        .await
        .unwrap_or_default()
}

pub(super) async fn record(home: &Path, key: Key) {
    let directory = home.join(DIRECTORY);
    let _ = tokio::task::spawn_blocking(move || write(&directory, key)).await;
}

fn validate_directory(directory: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(directory)?;
    anyhow::ensure!(metadata.is_dir(), "Invalid update cache directory");
    #[cfg(unix)]
    ensure_private(&metadata)?;
    #[cfg(windows)]
    paseo_client::windows_security::verify_private_directory(
        directory,
        paseo_client::windows_security::AllowedDirectoryUsers::CurrentUser,
    )?;
    Ok(())
}

#[cfg(unix)]
fn ensure_private(metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions.
    anyhow::ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
        "Update cache is not private to the current user"
    );
    Ok(())
}

fn read(directory: &Path) -> Result<Vec<Key>> {
    validate_directory(directory)?;
    let path = directory.join(FILE);
    anyhow::ensure!(
        fs::symlink_metadata(&path)?.is_file(),
        "Invalid update cache file"
    );
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() <= MAX_BYTES,
        "Invalid update cache file"
    );
    #[cfg(unix)]
    ensure_private(&metadata)?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "Update cache exceeds size limit"
    );
    let entries: Vec<Key> = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        entries.len() <= MAX_ENTRIES,
        "Too many update cache entries"
    );
    Ok(entries)
}

fn write(directory: &Path, key: Key) -> Result<()> {
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    match builder.create(directory) {
        Ok(()) => {
            #[cfg(windows)]
            paseo_client::windows_security::restrict_directory_to_current_user(directory)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    validate_directory(directory)?;
    let mut entries = read(directory).unwrap_or_default();
    entries.retain(|old| *old != key);
    entries.truncate(MAX_ENTRIES - 1);
    entries.insert(0, key);
    let bytes = serde_json::to_vec(&entries)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "Update cache exceeds size limit"
    );
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(directory.join(FILE))
        .map_err(|error| error.error)?;
    Ok(())
}

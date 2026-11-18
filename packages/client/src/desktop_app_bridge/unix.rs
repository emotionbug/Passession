use anyhow::{Result, ensure};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use tokio::net::{UnixListener, UnixStream};

const DIRECTORY: &str = ".app-ipc";
const SOCKET: &str = "v1.sock";

fn uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

fn directory(home: &Path, create: bool) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(home)?;
    ensure!(metadata.is_dir(), "Daemon home is not a real directory");
    ensure!(
        metadata.uid() == uid() && metadata.mode() & 0o022 == 0,
        "Daemon home is not controlled by this user"
    );
    let path = fs::canonicalize(home)?.join(DIRECTORY);
    if create {
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        metadata.is_dir() && metadata.uid() == uid() && metadata.mode() & 0o077 == 0,
        "Desktop IPC directory is not private to this user"
    );
    Ok(path)
}

fn owned_socket(metadata: &Metadata) -> bool {
    metadata.file_type().is_socket() && metadata.uid() == uid() && metadata.nlink() == 1
}

struct SocketFile {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            owned_socket(&metadata) && metadata.dev() == self.device && metadata.ino() == self.inode
        }) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(super) struct Listener {
    _socket_file: SocketFile,
    listener: UnixListener,
    _lock: File,
}

impl Listener {
    pub(super) async fn bind(home: &Path) -> Result<Self> {
        let home = home.to_owned();
        let (socket_file, listener, lock) = tokio::task::spawn_blocking(move || {
            let path = directory(&home, true)?;
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path.join("owner.lock"))?;
            let metadata = lock.metadata()?;
            ensure!(
                metadata.is_file()
                    && metadata.uid() == uid()
                    && metadata.mode() & 0o077 == 0
                    && metadata.nlink() == 1,
                "Invalid desktop IPC owner lock"
            );
            // The process lock survives until after unlinking our own socket. On a
            // crash it releases automatically, leaving only an inert socket inode.
            lock.try_lock()?;
            let path = path.join(SOCKET);
            match fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    ensure!(owned_socket(&metadata), "Invalid desktop IPC socket");
                    fs::remove_file(&path)?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            let listener = std::os::unix::net::UnixListener::bind(&path)?;
            let metadata = fs::symlink_metadata(&path)?;
            let socket_file = SocketFile {
                path,
                device: metadata.dev(),
                inode: metadata.ino(),
            };
            fs::set_permissions(&socket_file.path, fs::Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)?;
            Ok::<_, anyhow::Error>((socket_file, listener, lock))
        })
        .await??;
        Ok(Self {
            _socket_file: socket_file,
            listener: UnixListener::from_std(listener)?,
            _lock: lock,
        })
    }

    pub(super) async fn accept(&mut self) -> Result<Option<UnixStream>> {
        let (stream, _) = self.listener.accept().await?;
        Ok(stream
            .peer_cred()
            .is_ok_and(|cred| cred.uid() == uid())
            .then_some(stream))
    }
}

pub(super) async fn connect(home: &Path) -> Result<UnixStream> {
    let home = home.to_owned();
    let path = tokio::task::spawn_blocking(move || {
        let path = directory(&home, false)?.join(SOCKET);
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            owned_socket(&metadata) && metadata.mode() & 0o077 == 0,
            "Invalid desktop IPC socket"
        );
        Ok::<_, anyhow::Error>(path)
    })
    .await??;
    let stream = UnixStream::connect(&path).await?;
    ensure!(
        stream.peer_cred()?.uid() == uid(),
        "Desktop IPC peer belongs to another user"
    );
    // A moved socket entry must not route a request to a different daemon home.
    ensure!(
        stream.peer_addr()?.as_pathname() == Some(path.as_path()),
        "Desktop IPC peer is bound to another home"
    );
    Ok(stream)
}

#[cfg(test)]
pub(super) fn socket_path(home: &Path) -> Result<PathBuf> {
    Ok(directory(home, true)?.join(SOCKET))
}

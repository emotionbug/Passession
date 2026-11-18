use crate::windows_security::{create_private_named_pipe, verify_private_named_pipe};
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io,
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::Path,
};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::{
    Foundation::{ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED, HANDLE},
    Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_ID_INFO, FILE_READ_ATTRIBUTES, FileIdInfo, GetFileInformationByHandleEx,
        SECURITY_IDENTIFICATION,
    },
};

pub(super) async fn pipe_name(home: &Path) -> Result<String> {
    let home = home.to_owned();
    tokio::task::spawn_blocking(move || {
        let metadata = fs::symlink_metadata(&home)?;
        ensure!(
            metadata.is_dir() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "Daemon home is not a real directory"
        );
        let home = fs::canonicalize(home)?;
        let directory = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .security_qos_flags(SECURITY_IDENTIFICATION)
            .open(&home)?;
        let metadata = directory.metadata()?;
        ensure!(
            metadata.is_dir() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "Daemon home is not a real directory"
        );
        let mut identity = FILE_ID_INFO::default();
        // SAFETY: the directory handle is live and the output buffer matches FileIdInfo.
        if unsafe {
            GetFileInformationByHandleEx(
                directory.as_raw_handle() as HANDLE,
                FileIdInfo,
                (&mut identity as *mut FILE_ID_INFO).cast(),
                size_of::<FILE_ID_INFO>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        // Include the path to distinguish different shares and the filesystem
        // identity to distinguish a home replaced while the desktop is alive.
        let mut hash = Sha256::new();
        hash.update(identity.VolumeSerialNumber.to_le_bytes());
        hash.update(identity.FileId.Identifier);
        for unit in home.as_os_str().encode_wide() {
            hash.update(unit.to_le_bytes());
        }
        Ok(format!(
            r"\\.\pipe\paseo-desktop-app-v1-{:x}",
            hash.finalize()
        ))
    })
    .await?
}

fn instance(name: &str, first: bool) -> Result<NamedPipeServer> {
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true)
        .max_instances(super::MAX_ACTIVE + 1)
        .in_buffer_size(super::REQUEST_BYTES as u32)
        .out_buffer_size(super::RESPONSE_BYTES as u32);
    create_private_named_pipe(&options, name)
}

pub(super) struct Listener {
    name: String,
    pending: NamedPipeServer,
}

impl Listener {
    pub(super) async fn bind(home: &Path) -> Result<Self> {
        let name = pipe_name(home).await?;
        let pending = instance(&name, true)?;
        Ok(Self { name, pending })
    }

    pub(super) async fn accept(&mut self) -> Result<Option<NamedPipeServer>> {
        if let Err(error) = self.pending.connect().await {
            let disconnected = error.kind() == io::ErrorKind::BrokenPipe
                || matches!(error.raw_os_error(), Some(code)
                    if code == ERROR_NO_DATA as i32 || code == ERROR_PIPE_NOT_CONNECTED as i32);
            if disconnected {
                let replacement = instance(&self.name, false)?;
                self.pending = replacement;
                return Ok(None);
            }
            return Err(error.into());
        }
        let replacement = instance(&self.name, false)?;
        Ok(Some(std::mem::replace(&mut self.pending, replacement)))
    }
}

pub(super) async fn connect(home: &Path) -> Result<NamedPipeClient> {
    let name = pipe_name(home).await?;
    let client = ClientOptions::new()
        .security_qos_flags(SECURITY_IDENTIFICATION)
        .open(&name)?;
    verify_private_named_pipe(&client)?;
    Ok(client)
}

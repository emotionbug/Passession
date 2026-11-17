//! The caller authenticates the complete pinned package before entering this parser.
//! Only its XAR Payload is unpacked: pbzx -> bounded XZ chunks -> streaming odc cpio.
use anyhow::{Context, Result, bail, ensure};
use flate2::bufread::ZlibDecoder;
use lzma_rust2::XzReader;
use std::{
    fs::File,
    io::{self, BufReader, Cursor, Read, Seek, SeekFrom, Take},
    path::Path,
};

const MAX_TOC_COMPRESSED: u64 = 1024 * 1024;
const MAX_TOC_XML: u64 = 4 * 1024 * 1024;
const MAX_CHUNK: u64 = 16 * 1024 * 1024;
const MAX_CPIO_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const MAX_METADATA_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ENTRIES: usize = 100_000;

/// Extract the authenticated CLTools package into a directory that must not exist.
/// Does not execute installer scripts, change ownership, or preserve privileged bits.
pub(super) fn extract_sdk(archive: &Path, destination: &Path) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = (archive, destination);
        bail!("Apple SDK extraction requires a Unix host (SDK symlinks must be preserved)");
    }
    #[cfg(unix)]
    {
        let mut file = File::open(archive).context("Opening authenticated Apple SDK package")?;
        let (offset, length) = payload_range(&mut file).context("Reading Apple XAR Payload")?;
        file.seek(SeekFrom::Start(offset))?;
        let payload = PbzxReader::new(file.take(length))?;
        unix::extract_cpio(BufReader::with_capacity(64 * 1024, payload), destination)
            .context("Extracting Apple SDK cpio payload")
    }
}

fn be_u64(bytes: &[u8]) -> u64 {
    u64::from_be_bytes(bytes.try_into().expect("fixed-width binary header field"))
}

fn payload_range(file: &mut File) -> Result<(u64, u64)> {
    let file_size = file.metadata()?.len();
    let mut header = [0; 28];
    file.read_exact(&mut header)
        .context("Truncated XAR header")?;
    ensure!(
        &header[..4] == b"xar!",
        "Unsupported SDK archive: expected XAR"
    );
    ensure!(
        u16::from_be_bytes([header[4], header[5]]) == 28,
        "Unsupported XAR header size"
    );
    ensure!(
        u16::from_be_bytes([header[6], header[7]]) == 1,
        "Unsupported XAR version"
    );
    ensure!(
        u32::from_be_bytes(header[24..28].try_into()?) == 1,
        "Unsupported XAR TOC checksum algorithm"
    );
    let compressed_len = be_u64(&header[8..16]);
    let xml_len = be_u64(&header[16..24]);
    ensure!(
        (1..=MAX_TOC_COMPRESSED).contains(&compressed_len),
        "XAR compressed TOC exceeds limit"
    );
    ensure!(
        (1..=MAX_TOC_XML).contains(&xml_len),
        "XAR XML TOC exceeds limit"
    );
    let heap = 28 + compressed_len;
    ensure!(heap <= file_size, "Truncated XAR TOC");
    let mut compressed = vec![0; compressed_len as usize];
    file.read_exact(&mut compressed)?;
    let mut decoder = ZlibDecoder::new(compressed.as_slice());
    let mut xml = Vec::with_capacity(xml_len as usize);
    decoder.by_ref().take(xml_len + 1).read_to_end(&mut xml)?;
    ensure!(
        xml.len() as u64 == xml_len,
        "XAR TOC decoded length mismatch"
    );
    ensure!(
        decoder.total_in() == compressed_len,
        "Trailing compressed XAR TOC bytes"
    );
    let (offset, length) = parse_toc(std::str::from_utf8(&xml).context("XAR TOC is not UTF-8")?)?;
    let start = heap
        .checked_add(offset)
        .context("XAR Payload offset overflow")?;
    let end = start
        .checked_add(length)
        .context("XAR Payload length overflow")?;
    ensure!(end <= file_size, "XAR Payload extends beyond archive");
    Ok((start, length))
}

fn child<'a, 'input>(
    node: roxmltree::Node<'a, 'input>,
    name: &str,
) -> Result<roxmltree::Node<'a, 'input>> {
    let mut children = node.children().filter(|n| n.has_tag_name(name));
    let value = children
        .next()
        .with_context(|| format!("Missing XAR {name}"))?;
    ensure!(children.next().is_none(), "Duplicate XAR {name}");
    Ok(value)
}

fn decimal(node: roxmltree::Node<'_, '_>, name: &str) -> Result<u64> {
    let value = child(node, name)?
        .text()
        .context("Missing XAR numeric text")?;
    ensure!(
        !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
        "Invalid XAR {name}"
    );
    value
        .parse()
        .with_context(|| format!("XAR {name} exceeds u64"))
}

fn parse_toc(xml: &str) -> Result<(u64, u64)> {
    ensure!(xml.len() as u64 <= MAX_TOC_XML, "XAR XML TOC exceeds limit");
    let doc = roxmltree::Document::parse_with_options(
        xml,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 100_000,
        },
    )
    .context("Malformed XAR TOC XML")?;
    let root = doc.root_element();
    ensure!(root.has_tag_name("xar"), "Expected XAR XML root");
    let toc = child(root, "toc")?;
    let mut payload = None;
    for entry in toc.children().filter(|n| n.has_tag_name("file")) {
        if child(entry, "name")?.text() != Some("Payload") {
            continue;
        }
        ensure!(payload.is_none(), "Duplicate XAR Payload");
        ensure!(
            child(entry, "type")?.text() == Some("file"),
            "XAR Payload is not a file"
        );
        let data = child(entry, "data")?;
        ensure!(
            child(data, "encoding")?.attribute("style") == Some("application/octet-stream"),
            "Unsupported XAR Payload encoding"
        );
        let offset = decimal(data, "offset")?;
        let length = decimal(data, "length")?;
        ensure!(
            length >= 12 && decimal(data, "size")? == length,
            "Invalid XAR Payload size"
        );
        payload = Some((offset, length));
    }
    payload.context("XAR contains no top-level Payload")
}

struct PbzxReader<R: Read> {
    input: Take<R>,
    chunk: Cursor<Vec<u8>>,
    encoded: Vec<u8>,
    chunk_size: u64,
    decoded_total: u64,
}

impl<R: Read> PbzxReader<R> {
    fn new(mut input: Take<R>) -> Result<Self> {
        let mut header = [0; 12];
        input
            .read_exact(&mut header)
            .context("Truncated pbzx header")?;
        ensure!(
            &header[..4] == b"pbzx",
            "Unsupported XAR Payload: expected pbzx"
        );
        let chunk_size = be_u64(&header[4..]);
        ensure!(
            (1..=MAX_CHUNK).contains(&chunk_size),
            "Unsupported pbzx chunk size"
        );
        Ok(Self {
            input,
            chunk: Cursor::new(Vec::new()),
            encoded: Vec::new(),
            chunk_size,
            decoded_total: 0,
        })
    }

    fn next_chunk(&mut self) -> Result<bool> {
        if self.input.limit() == 0 {
            return Ok(false);
        }
        let mut header = [0; 16];
        self.input
            .read_exact(&mut header)
            .context("Truncated pbzx chunk header")?;
        let raw_len = be_u64(&header[..8]);
        let encoded_len = be_u64(&header[8..]);
        ensure!(
            (1..=self.chunk_size).contains(&raw_len),
            "Invalid pbzx decoded chunk length"
        );
        ensure!(
            (1..=MAX_CHUNK + 1024 * 1024).contains(&encoded_len),
            "pbzx encoded chunk exceeds limit"
        );
        ensure!(encoded_len <= self.input.limit(), "Truncated pbzx chunk");
        self.decoded_total = self
            .decoded_total
            .checked_add(raw_len)
            .context("pbzx size overflow")?;
        ensure!(
            self.decoded_total <= MAX_CPIO_BYTES,
            "pbzx decoded payload exceeds limit"
        );
        self.encoded.resize(encoded_len as usize, 0);
        self.input
            .read_exact(&mut self.encoded)
            .context("Truncated pbzx chunk data")?;
        let decoded = self.chunk.get_mut();
        decoded.clear();
        if self.encoded.starts_with(b"\xfd7zXZ\0") {
            // A hostile dictionary property must not allocate gigabytes before the
            // decoded-output limit can take effect. The pinned chunks need <64 MiB.
            let mut decoder =
                XzReader::new_mem_limit(Cursor::new(self.encoded.as_slice()), false, 64 * 1024);
            decoded.resize(raw_len as usize, 0);
            decoder
                .read_exact(decoded)
                .context("Truncated pbzx XZ decoded data")?;
            let mut extra = [0];
            ensure!(
                decoder.read(&mut extra).context("Invalid pbzx XZ footer")? == 0,
                "pbzx XZ decoded length mismatch"
            );
            ensure!(
                decoder.into_inner().position() == encoded_len,
                "Trailing bytes in pbzx XZ chunk"
            );
        } else {
            ensure!(encoded_len == raw_len, "Unsupported pbzx chunk encoding");
            std::mem::swap(decoded, &mut self.encoded);
        }
        self.chunk.set_position(0);
        Ok(true)
    }
}

impl<R: Read> Read for PbzxReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.chunk.position() == self.chunk.get_ref().len() as u64
            && !self.next_chunk().map_err(io::Error::other)?
        {
            return Ok(0);
        }
        self.chunk.read(buffer)
    }
}

fn octal(bytes: &[u8]) -> Result<u64> {
    ensure!(!bytes.is_empty(), "Empty cpio octal field");
    bytes.iter().try_fold(0u64, |value, &digit| {
        ensure!((b'0'..=b'7').contains(&digit), "Malformed cpio octal field");
        value
            .checked_mul(8)
            .and_then(|v| v.checked_add(u64::from(digit - b'0')))
            .context("cpio octal field overflow")
    })
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::{
        collections::{HashMap, VecDeque},
        ffi::{CStr, CString},
        fs::{self, DirBuilder},
        os::{
            fd::{AsRawFd, FromRawFd, RawFd},
            unix::{
                ffi::OsStrExt,
                fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
            },
        },
        path::{Component, PathBuf},
    };

    fn safe_path(name: &str) -> Result<&Path> {
        ensure!(
            !name.is_empty() && !name.contains(['\0', '\\']),
            "Unsafe cpio path"
        );
        let path = Path::new(name);
        ensure!(
            path.components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir)),
            "Unsafe cpio path: {name}"
        );
        Ok(path)
    }

    // Every directory is opened relative to an already anchored fd with NOFOLLOW.
    // Even a concurrent rename/symlink replacement cannot redirect a write outside
    // the extraction tree. cpio symlinks are created only after all regular files.
    fn directory(root: &File, path: &Path, create: bool) -> Result<File> {
        let mut current = None::<File>;
        let mut name = [0u8; MAX_PATH_BYTES + 1];
        for part in path.components() {
            let Component::Normal(part) = part else {
                continue;
            };
            let bytes = part.as_bytes();
            ensure!(
                bytes.len() <= MAX_PATH_BYTES,
                "cpio path component exceeds limit"
            );
            name[..bytes.len()].copy_from_slice(bytes);
            name[bytes.len()] = 0;
            let name = CStr::from_bytes_with_nul(&name[..=bytes.len()])?;
            let parent = current.as_ref().unwrap_or(root).as_raw_fd();
            if create {
                let result = unsafe { libc::mkdirat(parent, name.as_ptr(), 0o700) };
                if result != 0 {
                    let error = io::Error::last_os_error();
                    ensure!(
                        error.kind() == io::ErrorKind::AlreadyExists,
                        "Creating SDK directory: {error}"
                    );
                }
            }
            let fd = unsafe {
                libc::openat(
                    parent,
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            ensure!(
                fd >= 0,
                "Opening SDK directory without following symlinks: {}",
                io::Error::last_os_error()
            );
            current = Some(unsafe { File::from_raw_fd(fd) });
        }
        match current {
            Some(file) => Ok(file),
            None => root
                .try_clone()
                .context("Cloning SDK root directory handle"),
        }
    }

    fn leaf_name<'a>(path: &Path, buffer: &'a mut [u8; MAX_PATH_BYTES + 1]) -> Result<&'a CStr> {
        let bytes = path
            .file_name()
            .context("Missing cpio file name")?
            .as_bytes();
        ensure!(
            bytes.len() <= MAX_PATH_BYTES,
            "cpio file name exceeds limit"
        );
        buffer[..bytes.len()].copy_from_slice(bytes);
        buffer[bytes.len()] = 0;
        CStr::from_bytes_with_nul(&buffer[..=bytes.len()]).context("Invalid cpio file name")
    }

    fn regular_file(parent: RawFd, name: &CStr) -> Result<File> {
        let fd = unsafe {
            libc::openat(
                parent,
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        ensure!(
            fd >= 0,
            "Creating SDK file without replacing existing entries: {}",
            io::Error::last_os_error()
        );
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn validate_links(links: &HashMap<PathBuf, String>) -> Result<()> {
        for path in links.keys() {
            let mut pending: VecDeque<_> = path
                .to_str()
                .context("Non-UTF-8 SDK link path")?
                .split('/')
                .collect();
            let mut resolved = PathBuf::new();
            let mut expansions = 0;
            while let Some(part) = pending.pop_front() {
                match part {
                    "" | "." => continue,
                    ".." => ensure!(
                        resolved.pop(),
                        "SDK symlink escapes destination: {}",
                        path.display()
                    ),
                    part => resolved.push(part),
                }
                if let Some(target) = links.get(&resolved) {
                    expansions += 1;
                    ensure!(
                        expansions <= 64,
                        "Cyclic or excessively indirect SDK symlink: {}",
                        path.display()
                    );
                    resolved.pop();
                    for part in target.split('/').rev() {
                        pending.push_front(part);
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn extract_cpio(mut input: impl Read, destination: &Path) -> Result<()> {
        DirBuilder::new()
            .mode(0o700)
            .create(destination)
            .with_context(|| {
                format!("Creating absent SDK destination {}", destination.display())
            })?;
        let root = File::options()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(destination)?;
        let mut directories = Vec::new();
        let mut links = HashMap::new();
        let mut name = Vec::new();
        let mut leaf = [0; MAX_PATH_BYTES + 1];
        let mut entries = 0;
        let mut total = 0u64;
        let mut retained_metadata = 0u64;
        loop {
            let mut header = [0; 76];
            input
                .read_exact(&mut header)
                .context("Truncated odc cpio header (or missing trailer)")?;
            ensure!(
                &header[..6] == b"070707",
                "Unsupported cpio format: expected odc"
            );
            // Parse even unused metadata so malformed fields cannot pass silently.
            for field in header[6..48].chunks_exact(6) {
                octal(field)?;
            }
            octal(&header[48..59])?;
            let mode = octal(&header[18..24])? as u32;
            let nlink = octal(&header[36..42])?;
            let name_len = octal(&header[59..65])? as usize;
            let size = octal(&header[65..76])?;
            ensure!(
                (2..=MAX_PATH_BYTES + 1).contains(&name_len),
                "Invalid cpio path length"
            );
            ensure!(size <= MAX_FILE_BYTES, "cpio entry exceeds size limit");
            total = total
                .checked_add(76 + name_len as u64 + size)
                .context("cpio size overflow")?;
            ensure!(total <= MAX_CPIO_BYTES, "cpio payload exceeds size limit");
            name.resize(name_len, 0);
            input.read_exact(&mut name).context("Truncated cpio path")?;
            ensure!(
                name.last() == Some(&0) && !name[..name_len - 1].contains(&0),
                "Invalid cpio path terminator"
            );
            let name = std::str::from_utf8(&name[..name_len - 1]).context("Non-UTF-8 cpio path")?;
            if name == "TRAILER!!!" {
                ensure!(size == 0, "cpio trailer has data");
                break;
            }
            entries += 1;
            ensure!(entries <= MAX_ENTRIES, "cpio entry count exceeds limit");
            let path = safe_path(name)?;
            let permissions = fs::Permissions::from_mode(mode & 0o777);
            match mode & 0o170000 {
                0o040000 => {
                    ensure!(size == 0 && nlink >= 1, "Malformed cpio directory");
                    retained_metadata += name_len as u64;
                    ensure!(
                        retained_metadata <= MAX_METADATA_BYTES,
                        "cpio directory/link metadata exceeds limit"
                    );
                    directory(&root, path, true)?;
                    directories.push((path.to_owned(), permissions));
                }
                0o100000 => {
                    ensure!(nlink == 1, "Unsupported cpio hard-linked file");
                    let parent = directory(
                        &root,
                        path.parent().context("cpio file has no parent")?,
                        true,
                    )?;
                    let mut file = regular_file(parent.as_raw_fd(), leaf_name(path, &mut leaf)?)?;
                    let copied = io::copy(&mut input.by_ref().take(size), &mut file)?;
                    ensure!(copied == size, "Truncated cpio file data");
                    file.set_permissions(permissions)?;
                }
                0o120000 => {
                    ensure!(
                        nlink == 1 && (1..=MAX_PATH_BYTES as u64).contains(&size),
                        "Invalid cpio symlink size or link count"
                    );
                    retained_metadata += name_len as u64 + size;
                    ensure!(
                        retained_metadata <= MAX_METADATA_BYTES,
                        "cpio directory/link metadata exceeds limit"
                    );
                    ensure!(path.file_name().is_some(), "cpio root cannot be a symlink");
                    let mut target = vec![0; size as usize];
                    input
                        .read_exact(&mut target)
                        .context("Truncated cpio symlink")?;
                    let target = String::from_utf8(target).context("Non-UTF-8 cpio symlink")?;
                    ensure!(
                        !target.starts_with('/') && !target.contains(['\0', '\\']),
                        "Unsafe cpio symlink target"
                    );
                    // Normalize the leading './' to match paths encountered while
                    // resolving framework Versions/Current and parent-relative links.
                    let path: PathBuf = path
                        .components()
                        .filter(|c| !matches!(c, Component::CurDir))
                        .collect();
                    ensure!(
                        links.insert(path, target).is_none(),
                        "Duplicate cpio symlink"
                    );
                }
                _ => bail!("Unsupported cpio file type: {mode:o}"),
            }
        }
        // odc has no per-record alignment; only zero padding to a 512-byte block.
        let mut padding = [0; 512];
        let mut padding_len = 0;
        loop {
            let count = input
                .read(&mut padding)
                .context("Reading cpio padding and final pbzx footer")?;
            if count == 0 {
                break;
            }
            padding_len += count;
            ensure!(
                padding_len <= 511 && padding[..count].iter().all(|&b| b == 0),
                "Invalid trailing cpio data"
            );
        }
        validate_links(&links)?;
        for (path, target) in links {
            let parent = directory(
                &root,
                path.parent().context("cpio symlink has no parent")?,
                true,
            )?;
            let target = CString::new(target)?;
            let name = leaf_name(&path, &mut leaf)?;
            let result =
                unsafe { libc::symlinkat(target.as_ptr(), parent.as_raw_fd(), name.as_ptr()) };
            ensure!(
                result == 0,
                "Creating SDK symlink without replacing entries: {}",
                io::Error::last_os_error()
            );
        }
        // Delay chmod until descendants are complete, including potentially
        // read-only directories. Never apply setuid/setgid/sticky archive bits.
        for (path, permissions) in directories.into_iter().rev() {
            directory(&root, &path, false)?.set_permissions(permissions)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toc(data: &str) -> String {
        format!(
            "<xar><toc><file><name>Payload</name><type>file</type><data>{data}</data></file></toc></xar>"
        )
    }

    #[test]
    fn toc_requires_unique_unencoded_bounded_payload_metadata() {
        let data = "<offset>276</offset><length>12</length><size>12</size><encoding style=\"application/octet-stream\"/>";
        assert_eq!(parse_toc(&toc(data)).unwrap(), (276, 12));
        assert!(parse_toc(&toc(&data.replace("276", "-1"))).is_err());
        assert!(parse_toc(&toc(&data.replace("276", "18446744073709551616"))).is_err());
        assert!(parse_toc(&toc(&data.replace("octet-stream", "x-gzip"))).is_err());
        assert!(parse_toc(&toc(&format!("{data}<length>12</length>"))).is_err());
        assert!(
            parse_toc(&toc(data).replace("</toc>", "<file><name>Payload</name></file></toc>"))
                .is_err()
        );
        assert!(parse_toc("<!DOCTYPE xar [<!ENTITY x 'Payload'>]><xar><toc/></xar>").is_err());
        assert!(parse_toc("<xar><toc>").is_err());
    }

    fn xar(xml: &str, payload: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(xml.as_bytes()).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut bytes = b"xar!".to_vec();
        bytes.extend_from_slice(&28u16.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&(compressed.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&(xml.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&compressed);
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn xar_rejects_bad_headers_truncated_tocs_and_heap_ranges() {
        use std::io::Write;
        let xml = toc(
            "<offset>0</offset><length>12</length><size>12</size><encoding style=\"application/octet-stream\"/>",
        );
        let valid = xar(&xml, b"pbzx00000000");
        let parse = |bytes: &[u8]| {
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(bytes).unwrap();
            file.rewind().unwrap();
            payload_range(&mut file)
        };
        assert_eq!(parse(&valid).unwrap(), (valid.len() as u64 - 12, 12));
        for (start, replacement) in [
            (0, b"bad!".as_slice()),
            (4, [0, 27].as_slice()),
            (6, [0, 2].as_slice()),
            (24, [0, 0, 0, 2].as_slice()),
            (8, (MAX_TOC_COMPRESSED + 1).to_be_bytes().as_slice()),
            (16, (MAX_TOC_XML + 1).to_be_bytes().as_slice()),
        ] {
            let mut bytes = valid.clone();
            bytes[start..start + replacement.len()].copy_from_slice(replacement);
            assert!(parse(&bytes).is_err());
        }
        assert!(parse(&valid[..20]).is_err());
        assert!(parse(&valid[..30]).is_err());
        assert!(parse(&valid[..valid.len() - 1]).is_err());
        let overflow = toc(
            "<offset>18446744073709551615</offset><length>12</length><size>12</size><encoding style=\"application/octet-stream\"/>",
        );
        assert!(parse(&xar(&overflow, b"")).is_err());
    }

    fn pbzx(raw: u64, encoded: &[u8]) -> Vec<u8> {
        let mut result = b"pbzx".to_vec();
        result.extend_from_slice(&MAX_CHUNK.to_be_bytes());
        result.extend_from_slice(&raw.to_be_bytes());
        result.extend_from_slice(&(encoded.len() as u64).to_be_bytes());
        result.extend_from_slice(encoded);
        result
    }

    #[test]
    fn pbzx_checks_chunk_lengths_and_truncation() {
        let bytes = pbzx(3, b"abc");
        let mut reader = PbzxReader::new(Cursor::new(&bytes).take(bytes.len() as u64)).unwrap();
        let mut decoded = Vec::new();
        reader.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, b"abc");
        for bytes in [
            pbzx(4, b"abc"),
            pbzx(MAX_CHUNK + 1, b"x"),
            pbzx(1, b"\xfd7zXZ\0broken"),
        ] {
            let mut reader = PbzxReader::new(Cursor::new(&bytes).take(bytes.len() as u64)).unwrap();
            assert!(reader.read_to_end(&mut Vec::new()).is_err());
        }
        let truncated = &bytes[..bytes.len() - 1];
        let mut reader = PbzxReader::new(Cursor::new(truncated).take(bytes.len() as u64)).unwrap();
        assert!(reader.read_to_end(&mut Vec::new()).is_err());
    }

    #[test]
    fn pbzx_decodes_xz_and_checks_footer_and_exact_length() {
        let xz = hex::decode("fd377a585a000004e6d6b4460200210116000000742fe5a301000261626300002776271a4a09d82c00011b030b2fb9101fb6f37d010000000004595a").unwrap();
        let bytes = pbzx(3, &xz);
        let mut reader = PbzxReader::new(Cursor::new(&bytes).take(bytes.len() as u64)).unwrap();
        let mut decoded = Vec::new();
        reader.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, b"abc");
        let mut corrupt = xz.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        let mut extra = xz.clone();
        extra.push(0);
        for bytes in [
            pbzx(2, &xz),
            pbzx(4, &xz),
            pbzx(3, &corrupt),
            pbzx(3, &extra),
        ] {
            let mut reader = PbzxReader::new(Cursor::new(&bytes).take(bytes.len() as u64)).unwrap();
            assert!(reader.read_to_end(&mut Vec::new()).is_err());
        }
    }

    #[cfg(unix)]
    fn record(name: &str, mode: u32, data: &[u8]) -> Vec<u8> {
        let header = format!(
            "070707{:06o}{:06o}{mode:06o}{:06o}{:06o}{:06o}{:06o}{:011o}{:06o}{:011o}",
            0,
            1,
            0,
            0,
            1,
            0,
            0,
            name.len() + 1,
            data.len()
        );
        assert_eq!(header.len(), 76);
        let mut result = header.into_bytes();
        result.extend_from_slice(name.as_bytes());
        result.push(0);
        result.extend_from_slice(data);
        result
    }

    #[cfg(unix)]
    fn cpio(records: &[(&str, u32, &[u8])]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for &(name, mode, data) in records {
            bytes.extend(record(name, mode, data));
        }
        bytes.extend(record("TRAILER!!!", 0, b""));
        bytes
    }

    #[cfg(unix)]
    #[test]
    fn cpio_preserves_framework_links_and_file_modes() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("sdk");
        let bytes = cpio(&[
            (".", 0o040755, b""),
            (
                "./Foo.framework/Versions/A/Headers/Foo.h",
                0o100644,
                b"header",
            ),
            ("./Foo.framework/Versions/Current", 0o120755, b"A"),
            (
                "./Foo.framework/Headers",
                0o120755,
                b"Versions/Current/Headers",
            ),
            (
                "./usr/include/Foo.h",
                0o120755,
                b"../../Foo.framework/Headers/Foo.h",
            ),
            ("./tool", 0o100755, b"tool"),
        ]);
        let payload = pbzx(bytes.len() as u64, &bytes);
        let xml = toc(&format!(
            "<offset>0</offset><length>{0}</length><size>{0}</size><encoding style=\"application/octet-stream\"/>",
            payload.len()
        ));
        let package = xar(&xml, &payload);
        let mut archive = tempfile::NamedTempFile::new().unwrap();
        archive.write_all(&package).unwrap();
        extract_sdk(archive.path(), &destination).unwrap();
        assert_eq!(
            std::fs::read(destination.join("usr/include/Foo.h")).unwrap(),
            b"header"
        );
        assert_eq!(
            std::fs::read_link(destination.join("Foo.framework/Headers")).unwrap(),
            Path::new("Versions/Current/Headers")
        );
        assert_eq!(
            std::fs::metadata(destination.join("tool"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert!(unix::extract_cpio(bytes.as_slice(), &destination).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn cpio_rejects_unsafe_paths_symlink_chains_and_types() {
        let cases = [
            cpio(&[("../outside", 0o100644, b"bad")]),
            cpio(&[("/absolute", 0o100644, b"bad")]),
            cpio(&[("a/../../outside", 0o100644, b"bad")]),
            cpio(&[("link", 0o120777, b"../outside")]),
            cpio(&[("link", 0o120777, b"/outside")]),
            cpio(&[("a", 0o120777, b"."), ("link", 0o120777, b"a/../outside")]),
            cpio(&[("a", 0o120777, b"b"), ("b", 0o120777, b"a")]),
            cpio(&[("pipe", 0o010644, b"")]),
            cpio(&[("same", 0o100644, b"a"), ("same", 0o100644, b"b")]),
            cpio(&[("dir", 0o100644, b"a"), ("dir/file", 0o100644, b"b")]),
        ];
        for bytes in cases {
            let temp = tempfile::tempdir().unwrap();
            assert!(unix::extract_cpio(bytes.as_slice(), &temp.path().join("sdk")).is_err());
            assert!(!temp.path().join("outside").exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn cpio_rejects_malformed_and_truncated_records() {
        let valid = cpio(&[("file", 0o100644, b"content")]);
        let mut bad_octal = valid.clone();
        bad_octal[18] = b'8';
        let mut bad_name = valid.clone();
        bad_name[80] = b'x';
        let mut unsupported = valid.clone();
        unsupported[..6].copy_from_slice(b"070701");
        let mut hardlink = valid.clone();
        hardlink[36..42].copy_from_slice(b"000002");
        let mut too_large = valid.clone();
        too_large[65..76].copy_from_slice(b"77777777777");
        let mut trailing = valid.clone();
        trailing.push(1);
        let mut excess_padding = valid.clone();
        excess_padding.extend_from_slice(&[0; 512]);
        for bytes in [
            bad_octal,
            bad_name,
            unsupported,
            hardlink,
            too_large,
            trailing,
            excess_padding,
            valid[..10].to_vec(),
            valid[..79].to_vec(),
            valid[..84].to_vec(),
            valid[..88].to_vec(),
        ] {
            let temp = tempfile::tempdir().unwrap();
            assert!(unix::extract_cpio(bytes.as_slice(), &temp.path().join("sdk")).is_err());
        }
    }
}

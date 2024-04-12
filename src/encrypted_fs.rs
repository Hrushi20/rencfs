use std::{fs, io};
use std::cmp::{max, min};
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::fs::{File, OpenOptions, ReadDir};
use std::io::{Read, Seek, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;

use base64::decode;
use cryptostream::{read, write};
use fuser::{FileAttr, FileType};
use openssl::error::ErrorStack;
use openssl::symm::Cipher;
use rand::Rng;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::debug;

#[cfg(test)]
mod encrypted_fs_tests;
pub mod crypto_util;

pub(crate) const INODES_DIR: &str = "inodes";
pub(crate) const CONTENTS_DIR: &str = "contents";
pub(crate) const SECURITY_DIR: &str = "security";

pub(crate) const ROOT_INODE: u64 = 1;

#[derive(Error, Debug)]
pub enum FsError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("serialize error: {0}")]
    SerializeError(#[from] bincode::Error),

    #[error("item not found")]
    NotFound(String),

    #[error("inode not found")]
    InodeNotFound,

    #[error("invalid input")]
    InvalidInput(String),

    #[error("invalid node type")]
    InvalidInodeType,

    #[error("invalid file handle")]
    InvalidFileHandle,

    #[error("already exists")]
    AlreadyExists,

    #[error("not empty")]
    NotEmpty,

    #[error("other")]
    Other(String),

    #[error("encryption error: {0}")]
    Encryption(#[from] ErrorStack),
}

#[derive(Debug, PartialEq)]
pub struct DirectoryEntry {
    pub ino: u64,
    pub name: String,
    pub kind: FileType,
}

#[derive(Debug, PartialEq)]
pub struct DirectoryEntryPlus {
    pub ino: u64,
    pub name: String,
    pub kind: FileType,
    pub attr: FileAttr,
}

pub type FsResult<T> = Result<T, FsError>;

pub struct DirectoryEntryIterator(ReadDir, Vec<u8>);

impl Iterator for DirectoryEntryIterator {
    type Item = FsResult<DirectoryEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        let entry = self.0.next()?;
        if let Err(e) = entry {
            return Some(Err(e.into()));
        }
        let entry = entry.unwrap();
        let file = File::open(entry.path());
        if let Err(e) = file {
            return Some(Err(e.into()));
        }
        let file = file.unwrap();
        let mut name = entry.file_name().to_string_lossy().to_string();
        if name == "$." {
            name = ".".to_string();
        } else if name == "$.." {
            name = "..".to_string();
        } else {
            name = crypto_util::decrypt_and_unnormalize_end_file_name(&name, &self.1);
        }
        let res: bincode::Result<(u64, FileType)> = bincode::deserialize_from(crypto_util::create_decryptor(file, &self.1));
        if let Err(e) = res {
            return Some(Err(e.into()));
        }
        let (ino, kind): (u64, FileType) = res.unwrap();
        Some(Ok(DirectoryEntry {
            ino,
            name,
            kind,
        }))
    }
}

pub struct DirectoryEntryPlusIterator(ReadDir, PathBuf, Vec<u8>);

impl Iterator for DirectoryEntryPlusIterator {
    type Item = FsResult<DirectoryEntryPlus>;

    fn next(&mut self) -> Option<Self::Item> {
        let entry = self.0.next()?;
        if let Err(e) = entry {
            debug!("error reading directory entry: {:?}", e);
            return Some(Err(e.into()));
        }
        let entry = entry.unwrap();
        let file = File::open(entry.path());
        if let Err(e) = file {
            debug!("error opening file: {:?}", e);
            return Some(Err(e.into()));
        }
        let file = file.unwrap();
        let mut name = entry.file_name().to_string_lossy().to_string();
        if name == "$." {
            name = ".".to_string();
        } else if name == "$.." {
            name = "..".to_string();
        } else {
            name = crypto_util::decrypt_and_unnormalize_end_file_name(&name, &self.2);
        }
        let res: bincode::Result<(u64, FileType)> = bincode::deserialize_from(crypto_util::create_decryptor(file, &self.2));
        if let Err(e) = res {
            debug!("error deserializing directory entry: {:?}", e);
            return Some(Err(e.into()));
        }
        let (ino, kind): (u64, FileType) = res.unwrap();

        let file = File::open(&self.1.join(ino.to_string()));
        if let Err(e) = file {
            debug!("error opening file: {:?}", e);
            return Some(Err(e.into()));
        }
        let file = file.unwrap();
        let attr = bincode::deserialize_from(crypto_util::create_decryptor(file, &self.2));
        if let Err(e) = attr {
            debug!("error deserializing file attr: {:?}", e);
            return Some(Err(e.into()));
        }
        let attr = attr.unwrap();
        Some(Ok(DirectoryEntryPlus {
            ino,
            name,
            kind,
            attr,
        }))
    }
}

pub struct EncryptedFs {
    pub data_dir: PathBuf,
    write_handles: BTreeMap<u64, (FileAttr, PathBuf, u64, write::Encryptor<File>)>,
    read_handles: BTreeMap<u64, (FileAttr, u64, read::Decryptor<File>)>,
    current_handle: AtomicU64,
    key: Vec<u8>,
}

impl EncryptedFs {
    pub fn new(data_dir: &str, password: &str) -> FsResult<Self> {
        let path = PathBuf::from(&data_dir);

        ensure_structure_created(&path)?;

        let mut fs = EncryptedFs {
            data_dir: path,
            write_handles: BTreeMap::new(),
            read_handles: BTreeMap::new(),
            current_handle: AtomicU64::new(1),
            key: crypto_util::derive_key(password, "salt-42"),
        };
        let _ = fs.ensure_root_exists();

        Ok(fs)
    }

    pub fn node_exists(&self, ino: u64) -> bool {
        let path = self.data_dir.join(INODES_DIR).join(ino.to_string());
        path.is_file()
    }

    pub fn is_dir(&self, ino: u64) -> bool {
        let path = self.data_dir.join(CONTENTS_DIR).join(ino.to_string());
        path.is_dir()
    }

    pub fn is_file(&self, ino: u64) -> bool {
        let path = self.data_dir.join(CONTENTS_DIR).join(ino.to_string());
        path.is_file()
    }

    /// Create a new node in the filesystem
    /// You don't need to provide `attr.ino`, it will be auto-generated anyway.
    pub fn create_nod(&mut self, parent: u64, name: &str, mut attr: FileAttr, read: bool, write: bool) -> FsResult<(u64, FileAttr)> {
        if !self.node_exists(parent) {
            return Err(FsError::InodeNotFound);
        }
        if self.find_by_name(parent, name)?.is_some() {
            return Err(FsError::AlreadyExists);
        }

        attr.ino = self.generate_next_inode();

        // write inode
        self.write_inode(&attr)?;

        // create in contents directory
        match attr.kind {
            FileType::RegularFile => {
                let path = self.data_dir.join(CONTENTS_DIR).join(attr.ino.to_string());
                // create the file
                OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&path)?;
            }
            FileType::Directory => {
                // create the directory
                fs::create_dir(self.data_dir.join(CONTENTS_DIR).join(attr.ino.to_string()))?;

                // add "." and ".." entries
                self.insert_directory_entry(attr.ino, DirectoryEntry {
                    ino: attr.ino,
                    name: "$.".to_string(),
                    kind: FileType::Directory,
                })?;
                self.insert_directory_entry(attr.ino, DirectoryEntry {
                    ino: parent,
                    name: "$..".to_string(),
                    kind: FileType::Directory,
                })?;
            }
            _ => { return Err(FsError::InvalidInodeType); }
        }

        // edd entry in parent directory, used for listing
        self.insert_directory_entry(parent, DirectoryEntry {
            ino: attr.ino,
            name: name.to_string(),
            kind: attr.kind,
        })?;

        let mut parent_attr = self.get_inode(parent)?;
        parent_attr.mtime = std::time::SystemTime::now();
        parent_attr.ctime = std::time::SystemTime::now();
        self.write_inode(&parent_attr)?;

        let handle = if attr.kind == FileType::RegularFile {
            if read || write {
                self.open(attr.ino, read, write)?
            } else {
                // we don't create handle for files that are not opened
                0
            }
        } else {
            // we don't use handle for directories
            0
        };

        Ok((handle, attr))
    }

    pub fn find_by_name(&self, parent: u64, mut name: &str) -> FsResult<Option<FileAttr>> {
        if !self.node_exists(parent) {
            return Err(FsError::InodeNotFound);
        }
        if !self.exists_by_name(parent, name) {
            return Ok(None);
        }
        if !self.is_dir(parent) {
            return Err(FsError::InvalidInodeType);
        }
        if name == "." {
            name = "$.";
        } else if name == ".." {
            name = "$..";
        }
        let name = crypto_util::normalize_end_encrypt_file_name(name, &self.key);
        let file = File::open(self.data_dir.join(CONTENTS_DIR).join(parent.to_string()).join(name))?;
        let (inode, _): (u64, FileType) = bincode::deserialize_from(crypto_util::create_decryptor(file, &self.key))?;
        Ok(Some(self.get_inode(inode)?))
    }

    pub fn children_count(&self, ino: u64) -> FsResult<usize> {
        let iter = self.read_dir(ino)?;
        Ok(iter.into_iter().count())
    }

    pub fn remove_dir(&mut self, parent: u64, name: &str) -> FsResult<()> {
        if !self.is_dir(parent) {
            return Err(FsError::InvalidInodeType);
        }

        if !self.exists_by_name(parent, name) {
            return Err(FsError::NotFound("name not found".to_string()));
        }

        let attr = self.find_by_name(parent, name)?.ok_or(FsError::NotFound("name not found".to_string()))?;
        if !matches!(attr.kind, FileType::Directory) {
            return Err(FsError::InvalidInodeType);
        }
        // check if it's empty
        let iter = self.read_dir(attr.ino)?;
        let count_children = iter.into_iter().take(3).count();
        if count_children > 2 {
            return Err(FsError::NotEmpty);
        }

        let ino_str = attr.ino.to_string();
        // remove inode file
        fs::remove_file(self.data_dir.join(INODES_DIR).join(&ino_str))?;
        // remove contents directory
        fs::remove_dir_all(self.data_dir.join(CONTENTS_DIR).join(&ino_str))?;
        // remove from parent directory
        let name = crypto_util::normalize_end_encrypt_file_name(name, &self.key);
        fs::remove_file(self.data_dir.join(CONTENTS_DIR).join(parent.to_string()).join(name))?;

        let mut parent_attr = self.get_inode(parent)?;
        parent_attr.mtime = std::time::SystemTime::now();
        parent_attr.ctime = std::time::SystemTime::now();
        self.write_inode(&parent_attr)?;

        Ok(())
    }

    pub fn remove_file(&mut self, parent: u64, name: &str) -> FsResult<()> {
        if !self.is_dir(parent) {
            return Err(FsError::InvalidInodeType);
        }
        if !self.exists_by_name(parent, name) {
            return Err(FsError::NotFound("name not found".to_string()));
        }

        let attr = self.find_by_name(parent, name)?.ok_or(FsError::NotFound("name not found".to_string()))?;
        if !matches!(attr.kind, FileType::RegularFile) {
            return Err(FsError::InvalidInodeType);
        }
        let ino_str = attr.ino.to_string();

        // remove inode file
        fs::remove_file(self.data_dir.join(INODES_DIR).join(&ino_str))?;
        // remove contents file
        fs::remove_file(self.data_dir.join(CONTENTS_DIR).join(&ino_str))?;
        // remove from parent directory
        let name = crypto_util::normalize_end_encrypt_file_name(name, &self.key);
        fs::remove_file(self.data_dir.join(CONTENTS_DIR).join(parent.to_string()).join(name))?;

        let mut parent_attr = self.get_inode(parent)?;
        parent_attr.mtime = std::time::SystemTime::now();
        parent_attr.ctime = std::time::SystemTime::now();
        self.write_inode(&parent_attr)?;

        Ok(())
    }

    pub fn exists_by_name(&self, parent: u64, mut name: &str) -> bool {
        if name == "." {
            name = "$.";
        } else if name == ".." {
            name = "$..";
        }
        let name = crypto_util::normalize_end_encrypt_file_name(name, &self.key);
        self.data_dir.join(CONTENTS_DIR).join(parent.to_string()).join(name).exists()
    }

    pub fn read_dir(&self, ino: u64) -> FsResult<DirectoryEntryIterator> {
        let contents_dir = self.data_dir.join(CONTENTS_DIR).join(ino.to_string());
        if !contents_dir.is_dir() {
            return Err(FsError::InvalidInodeType);
        }

        let iter = fs::read_dir(contents_dir)?;
        Ok(DirectoryEntryIterator(iter.into_iter(), self.key.clone()))
    }

    pub fn read_dir_plus(&self, ino: u64) -> FsResult<DirectoryEntryPlusIterator> {
        let contents_dir = self.data_dir.join(CONTENTS_DIR).join(ino.to_string());
        if !contents_dir.is_dir() {
            return Err(FsError::InvalidInodeType);
        }

        let iter = fs::read_dir(contents_dir)?;
        Ok(DirectoryEntryPlusIterator(iter.into_iter(), self.data_dir.join(INODES_DIR), self.key.clone()))
    }

    pub fn get_inode(&self, ino: u64) -> FsResult<FileAttr> {
        let path = self.data_dir.join(INODES_DIR).join(ino.to_string());
        if let Ok(file) = OpenOptions::new().read(true).write(true).open(path) {
            Ok(bincode::deserialize_from(crypto_util::create_decryptor(file, &self.key))?)
        } else {
            Err(FsError::InodeNotFound)
        }
    }

    pub fn replace_inode(&mut self, ino: u64, attr: &mut FileAttr) -> FsResult<()> {
        if !self.node_exists(ino) {
            return Err(FsError::InodeNotFound);
        }
        if !matches!(attr.kind, FileType::Directory) && !matches!(attr.kind, FileType::RegularFile) {
            return Err(FsError::InvalidInodeType);
        }

        attr.ctime = std::time::SystemTime::now();

        self.write_inode(attr)
    }

    pub fn read(&mut self, ino: u64, offset: u64, mut buf: &mut [u8], handle: u64) -> FsResult<usize> {
        if !self.node_exists(ino) {
            return Err(FsError::InodeNotFound);
        }
        if !self.is_file(ino) {
            return Err(FsError::InvalidInodeType);
        }
        if !self.read_handles.contains_key(&handle) {
            return Err(FsError::InvalidFileHandle);
        }
        let (attr, position, _) = self.read_handles.get(&handle).unwrap();
        if attr.ino != ino {
            return Err(FsError::InvalidFileHandle);
        }
        if matches!(attr.kind, FileType::Directory) {
            return Err(FsError::InvalidInodeType);
        }
        if offset >= attr.size {
            // if we need an offset after file size we don't read nothing
            return Ok(0);
        }

        if *position != offset {
            // in order to seek we need to read the bytes from current position until the offset
            if *position > offset {
                // if we need an offset before the current position, we can't seek back, we need
                // to read from the beginning until the desired offset
                self.create_read_handle(ino, handle)?;
            }
            if offset > 0 {
                let (_, position, decryptor) =
                    self.read_handles.get_mut(&handle).unwrap();
                let mut buffer: [u8; 4096] = [0; 4096];
                loop {
                    let read_len = if *position + buffer.len() as u64 > offset {
                        (offset - *position) as usize
                    } else {
                        buffer.len()
                    };
                    if read_len > 0 {
                        decryptor.read_exact(&mut buffer[..read_len])?;
                        *position += read_len as u64;
                        if *position == offset {
                            break;
                        }
                    }
                }
            }
        }
        let (attr, position, decryptor) =
            self.read_handles.get_mut(&handle).unwrap();
        if offset + buf.len() as u64 > attr.size {
            buf = &mut buf[..(attr.size - offset) as usize];
        }
        decryptor.read_exact(&mut buf)?;
        *position += buf.len() as u64;

        attr.atime = std::time::SystemTime::now();

        Ok(buf.len())
    }

    pub fn release_handle(&mut self, handle: u64) -> FsResult<()> {
        if handle == 0 {
            // in case of directory or if the file was crated without being opened we don't use handle
            return Ok(());
        }
        let mut valid_fh = false;
        if let Some((attr, _, decryptor)) = self.read_handles.remove(&handle) {
            // write attr only here to avoid serializing it multiple times while reading
            self.write_inode(&attr)?;
            decryptor.finish();
            valid_fh = true;
        }
        if let Some((attr, path, _, encryptor)) = self.write_handles.remove(&handle) {
            // write attr only here to avoid serializing it multiple times while writing
            self.write_inode(&attr)?;
            encryptor.finish()?;
            // if we are in tmp file move it to actual file
            if path.to_str().unwrap().ends_with(".tmp") {
                fs::rename(path, self.data_dir.join(CONTENTS_DIR).join(attr.ino.to_string())).unwrap();

                // also recreate readers because the file has changed
                let keys: Vec<u64> = self.read_handles.keys().cloned().collect();
                for key in keys {
                    let (attr, _, _) = self.read_handles.remove(&key).unwrap();
                    self.create_read_handle(attr.ino, key).unwrap();
                }
            }
            valid_fh = true;
        }
        if !valid_fh {
            return Err(FsError::InvalidFileHandle);
        }
        Ok(())
    }

    pub fn is_read_handle(&self, fh: u64) -> bool {
        self.read_handles.contains_key(&fh)
    }

    pub fn is_write_handle(&self, fh: u64) -> bool {
        self.write_handles.contains_key(&fh)
    }

    pub fn write_all(&mut self, ino: u64, offset: u64, buf: &[u8], handle: u64) -> FsResult<()> {
        if !self.node_exists(ino) {
            return Err(FsError::InodeNotFound);
        }
        if !self.is_file(ino) {
            return Err(FsError::InvalidInodeType);
        }
        if !self.write_handles.contains_key(&handle) {
            return Err(FsError::InvalidFileHandle);
        }
        let (attr, _, position, _) =
            self.write_handles.get_mut(&handle).unwrap();
        if attr.ino != ino {
            return Err(FsError::InvalidFileHandle);
        }
        if matches!(attr.kind, FileType::Directory) {
            return Err(FsError::InvalidInodeType);
        }

        if *position != offset {
            // in order to seek we need to recreate all stream from the beginning until the desired position of file size
            // for that we create a new encryptor into a tmp file reading from original file and writing to tmp one
            // when we release the handle we will move this tmp file to the actual file

            // remove handle data because we will replace it with the tmp one
            let (attr, path, mut position, encryptor) =
                self.write_handles.remove(&handle).unwrap();

            // finish the current writer so we flush all data to the file
            encryptor.finish()?;

            // if we are already in the tmp file first copy tmp to actual file
            if path.to_str().unwrap().ends_with(".tmp") {
                fs::rename(path, self.data_dir.join(CONTENTS_DIR).join(attr.ino.to_string())).unwrap();
            }

            let in_path = self.data_dir.join(CONTENTS_DIR).join(attr.ino.to_string());
            let in_file = OpenOptions::new().read(true).write(true).open(in_path.clone())?;

            let tmp_path_str = format!("{}.{}.tmp", attr.ino.to_string(), &handle.to_string());
            let tmp_path = self.data_dir.join(CONTENTS_DIR).join(tmp_path_str);
            let tmp_file = OpenOptions::new().read(true).write(true).create(true).open(tmp_path.clone())?;

            let mut decryptor = crypto_util::create_decryptor(in_file, &self.key);
            let mut encryptor = crypto_util::create_encryptor(tmp_file, &self.key);

            let mut buffer: [u8; 4096] = [0; 4096];
            let mut pos_read = 0;
            position = 0;
            if offset > 0 {
                loop {
                    let offset_in_bounds = min(offset, attr.size); // keep offset in bounds of file
                    let read_len = if pos_read + buffer.len() as u64 > offset_in_bounds {
                        (offset_in_bounds - pos_read) as usize
                    } else {
                        buffer.len()
                    };
                    if read_len > 0 {
                        decryptor.read_exact(&mut buffer[..read_len])?;
                        encryptor.write_all(&buffer[..read_len])?;
                        pos_read += read_len as u64;
                        position += read_len as u64;
                        if pos_read == offset_in_bounds {
                            break;
                        }
                    }
                }
            }
            self.replace_handle_data(handle, attr, tmp_path, position, encryptor);
        }
        let (attr, _, position, encryptor) =
            self.write_handles.get_mut(&handle).unwrap();

        // if offset is after current position (max file size) we fill up with zeros until offset
        if offset > *position {
            let buffer: [u8; 4096] = [0; 4096];
            loop {
                let len = min(4096, offset - *position) as usize;
                encryptor.write_all(&buffer[..len])?;
                *position += len as u64;
                if *position == offset {
                    break;
                }
            }
        }

        // now write the new data
        encryptor.write_all(buf)?;
        *position += buf.len() as u64;

        // if position is before file end we copy the rest of the file from position to the end
        if *position < attr.size {
            let mut buffer: [u8; 4096] = [0; 4096];
            let mut decryptor = crypto_util::create_decryptor(OpenOptions::new().read(true).open(self.data_dir.join(CONTENTS_DIR).join(attr.ino.to_string()))?, &self.key);
            // move read position to the desired position
            loop {
                let mut read_pos = 0u64;
                let len = min(4096, *position - read_pos) as usize;
                decryptor.read_exact(&mut buffer[..len])?;
                read_pos += len as u64;
                if read_pos == *position {
                    break;
                }
            }
            // copy the rest of the file
            loop {
                let len = min(4096, attr.size - *position) as usize;
                decryptor.read_exact(&mut buffer[..len])?;
                encryptor.write_all(&buffer[..len])?;
                *position += len as u64;
                if *position == attr.size {
                    break;
                }
            }
            decryptor.finish();
        }

        let size = *position;
        attr.size = size;
        attr.mtime = std::time::SystemTime::now();
        attr.ctime = std::time::SystemTime::now();

        Ok(())
    }

    pub fn flush(&mut self, handle: u64) -> FsResult<()> {
        if handle == 0 {
            // in case of directory or if the file was crated without being opened we don't use handle
            return Ok(());
        }
        if !self.write_handles.contains_key(&handle) {
            return Err(FsError::InvalidFileHandle);
        }
        if let Some((_, _, _, encryptor)) = self.write_handles.get_mut(&handle) {
            encryptor.flush()?;
        }
        Ok(())
    }

    pub fn copy_file_range(&mut self, src_ino: u64, src_offset: u64, dest_ino: u64, dest_offset: u64, size: usize, src_fh: u64, dest_fh: u64) -> FsResult<usize> {
        if self.is_dir(src_ino) || self.is_dir(dest_ino) {
            return Err(FsError::InvalidInodeType);
        }

        let mut buf = vec![0; size];
        let len = self.read(src_ino, src_offset, &mut buf, src_fh)?;
        self.write_all(dest_ino, dest_offset, &buf[..len], dest_fh)?;

        Ok(len)
    }

    /// Open a file.
    pub fn open(&mut self, ino: u64, read: bool, write: bool) -> FsResult<u64> {
        if !read && !write {
            return Err(FsError::InvalidInput("read and write cannot be false at the same time".to_string()));
        }
        if self.is_dir(ino) {
            return Err(FsError::InvalidInodeType);
        }

        let mut handle = 0u64;
        if read {
            handle = self.allocate_next_handle();
            self.create_read_handle(ino, handle)?;
        }
        if write {
            if self.write_handles.contains_key(&handle) {
                return Err(FsError::InvalidInput("write handle already opened".to_string()));
            }
            handle = self.allocate_next_handle();
            self.create_write_handle(ino, handle)?;
        }
        Ok(handle)
    }

    pub fn truncate(&mut self, ino: u64, size: u64) -> FsResult<()> {
        let mut attr = self.get_inode(ino)?;
        if matches!(attr.kind, FileType::Directory) {
            return Err(FsError::InvalidInodeType);
        }

        if (size == attr.size) {
            // no-op
            return Ok(());
        } else if size == 0 {
            // truncate to zero
            OpenOptions::new().write(true).create(true).truncate(true).open(self.data_dir.join(CONTENTS_DIR).join(ino.to_string()))?;
        } else if size < attr.size {
            // decrease size, copy from beginning until size as offset
            // TODO
            let fh = self.open(ino, false, true)?;
            self.write_all(ino, size, &[], fh)?;
            self.release_handle(fh)?;
        } else {
            // increase size, write zeros from actual size to new size
            let fh = self.open(ino, false, true)?;
            let buf: [u8; 4096] = [0; 4096];
            loop {
                let len = min(4096, size - attr.size) as usize;
                self.write_all(ino, attr.size, &buf[..len], fh)?;
                attr.size += len as u64;
                if attr.size == size {
                    break;
                }
            }
            self.flush(fh)?;
            self.release_handle(fh)?;
        }

        attr.size = size;
        attr.mtime = std::time::SystemTime::now();
        attr.ctime = std::time::SystemTime::now();
        self.write_inode(&attr)?;

        Ok(())
    }

    pub fn rename(&mut self, parent: u64, name: &str, new_parent: u64, new_name: &str) -> FsResult<()> {
        if !self.node_exists(parent) {
            return Err(FsError::InodeNotFound);
        }
        if !self.is_dir(parent) {
            return Err(FsError::InvalidInodeType);
        }
        if !self.node_exists(new_parent) {
            return Err(FsError::InodeNotFound);
        }
        if !self.is_dir(new_parent) {
            return Err(FsError::InvalidInodeType);
        }
        if !self.exists_by_name(parent, name) {
            return Err(FsError::NotFound("name not found".to_string()));
        }

        if parent == new_parent && name == new_name {
            // no-op
            return Ok(());
        }

        // Only overwrite an existing directory if it's empty
        if let Ok(Some(new_attr)) = self.find_by_name(new_parent, new_name) {
            if new_attr.kind == FileType::Directory &&
                self.children_count(new_attr.ino)? > 2 {
                return Err(FsError::NotEmpty);
            }
        }

        let mut attr = self.find_by_name(parent, name)?.unwrap();
        // remove from parent contents
        self.remove_directory_entry(parent, name)?;
        // add to new parent contents
        self.insert_directory_entry(new_parent, DirectoryEntry {
            ino: attr.ino,
            name: new_name.to_string(),
            kind: attr.kind,
        })?;

        let mut parent_attr = self.get_inode(parent)?;
        parent_attr.mtime = std::time::SystemTime::now();
        parent_attr.ctime = std::time::SystemTime::now();

        let mut new_parent_attr = self.get_inode(new_parent)?;
        new_parent_attr.mtime = std::time::SystemTime::now();
        new_parent_attr.ctime = std::time::SystemTime::now();

        attr.ctime = std::time::SystemTime::now();

        if attr.kind == FileType::Directory {
            // add parent link to new directory
            self.insert_directory_entry(attr.ino, DirectoryEntry {
                ino: new_parent,
                name: "$..".to_string(),
                kind: FileType::Directory,
            })?;
        }

        Ok(())
    }

    pub(crate) fn write_inode(&mut self, attr: &FileAttr) -> FsResult<()> {
        let path = self.data_dir.join(INODES_DIR).join(attr.ino.to_string());
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        Ok(bincode::serialize_into(crypto_util::create_encryptor(file, &self.key), &attr)?)
    }

    pub fn allocate_next_handle(&mut self) -> u64 {
        self.current_handle.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }

    pub fn create_encryptor(&self, file: File) -> write::Encryptor<File> {
        crypto_util::create_encryptor(file, &self.key)
    }

    pub fn create_decryptor(&self, file: File) -> read::Decryptor<File> {
        crypto_util::create_decryptor(file, &self.key)
    }

    pub fn encrypt_string(&self, s: &str) -> String {
        crypto_util::encrypt_string(s, &self.key)
    }

    pub fn decrypt_string(&self, s: &str) -> String {
        crypto_util::decrypt_string(s, &self.key)
    }

    fn create_read_handle(&mut self, ino: u64, handle: u64) -> FsResult<u64> {
        let path = self.data_dir.join(CONTENTS_DIR).join(ino.to_string());
        let file = OpenOptions::new().read(true).write(true).open(path)?;

        let decryptor = crypto_util::create_decryptor(file, &self.key);
        let attr = self.get_inode(ino)?;
        // save attr also to avoid loading it multiple times while reading
        self.read_handles.insert(handle, (attr, 0, decryptor));
        Ok(handle)
    }

    fn create_write_handle(&mut self, ino: u64, handle: u64) -> FsResult<u64> {
        let path = self.data_dir.join(CONTENTS_DIR).join(ino.to_string());
        let file = OpenOptions::new().read(true).write(true).open(path.clone())?;

        let encryptor = crypto_util::create_encryptor(file, &self.key);
        // save attr also to avoid loading it multiple times while writing
        let attr = self.get_inode(ino)?;
        self.write_handles.insert(handle, (attr, path, 0, encryptor));
        Ok(handle)
    }

    fn replace_handle_data(&mut self, handle: u64, attr: FileAttr, new_path: PathBuf, position: u64, new_encryptor: write::Encryptor<File>) {
        self.write_handles.insert(handle, (attr, new_path, position, new_encryptor));
    }

    fn ensure_root_exists(&mut self) -> FsResult<()> {
        if !self.node_exists(ROOT_INODE) {
            let mut attr = FileAttr {
                ino: ROOT_INODE,
                size: 0,
                blocks: 0,
                atime: std::time::SystemTime::now(),
                mtime: std::time::SystemTime::now(),
                ctime: std::time::SystemTime::now(),
                crtime: std::time::SystemTime::now(),
                kind: FileType::Directory,
                perm: 0o755,
                nlink: 2,
                uid: 0,
                gid: 0,
                rdev: 0,
                blksize: 0,
                flags: 0,
            };
            #[cfg(target_os = "linux")]
            {
                use std::os::unix::fs::MetadataExt;
                let metadata = fs::metadata(self.data_dir.clone())?;
                attr.uid = metadata.uid();
                attr.gid = metadata.gid();
            }

            self.write_inode(&attr)?;

            // create the directory
            fs::create_dir(self.data_dir.join(CONTENTS_DIR).join(attr.ino.to_string()))?;

            // add "." entry
            self.insert_directory_entry(attr.ino, DirectoryEntry {
                ino: attr.ino,
                name: "$.".to_string(),
                kind: FileType::Directory,
            })?;
        }

        Ok(())
    }

    fn insert_directory_entry(&self, parent: u64, entry: DirectoryEntry) -> FsResult<()> {
        let parent_path = self.data_dir.join(CONTENTS_DIR).join(parent.to_string());
        // remove path separators from name
        let name = crypto_util::normalize_end_encrypt_file_name(&entry.name, &self.key);
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&parent_path.join(name))?;

        // write inode and file type
        let entry = (entry.ino, entry.kind);
        bincode::serialize_into(crypto_util::create_encryptor(file, &self.key), &entry)?;

        Ok(())
    }

    fn remove_directory_entry(&self, parent: u64, name: &str) -> FsResult<()> {
        let parent_path = self.data_dir.join(CONTENTS_DIR).join(parent.to_string());
        let name = crypto_util::normalize_end_encrypt_file_name(name, &self.key);
        fs::remove_file(parent_path.join(name))?;
        Ok(())
    }

    fn generate_next_inode(&self) -> u64 {
        loop {
            let mut rng = rand::thread_rng();
            let ino = rng.gen::<u64>();

            if ino <= ROOT_INODE {
                continue;
            }
            if self.node_exists(ino) {
                continue;
            }

            return ino;
        }
    }
}

fn ensure_structure_created(data_dir: &PathBuf) -> FsResult<()> {
    if !data_dir.exists() {
        fs::create_dir_all(&data_dir)?;
    }

    // create directories
    let dirs = vec![INODES_DIR, CONTENTS_DIR, SECURITY_DIR];
    for dir in dirs {
        let path = data_dir.join(dir);
        if !path.exists() {
            fs::create_dir_all(path)?;
        }
    }

    Ok(())
}
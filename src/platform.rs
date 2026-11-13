//! Filesystem metadata without following links or opening file contents.
use std::{fs, io, path::Path};

pub struct Metadata {
    identity: (u64, u64),
    links: u64,
    apparent: u64,
    allocated: u64,
    directory: bool,
    link: bool,
    #[cfg(windows)]
    directory_link: bool,
}

impl Metadata {
    pub fn identity(&self) -> (u64, u64) {
        self.identity
    }

    pub fn links(&self) -> u64 {
        self.links
    }

    pub fn len(&self) -> u64 {
        self.apparent
    }

    pub fn allocated(&self) -> u64 {
        self.allocated
    }

    pub fn is_dir(&self) -> bool {
        self.directory
    }

    pub fn is_symlink(&self) -> bool {
        self.link
    }
}

#[cfg(unix)]
pub fn metadata(path: &Path) -> io::Result<Metadata> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path)?;
    Ok(Metadata {
        identity: (metadata.dev(), metadata.ino()),
        links: metadata.nlink(),
        apparent: metadata.len(),
        allocated: metadata.blocks().saturating_mul(512),
        directory: metadata.is_dir(),
        link: metadata.file_type().is_symlink(),
    })
}

/// Remove only this name. Windows directory links require RemoveDirectory.
pub fn unlink(path: &Path, metadata: &Metadata) -> io::Result<()> {
    #[cfg(windows)]
    if metadata.directory_link {
        return fs::remove_dir(path);
    }
    let _ = metadata;
    fs::remove_file(path)
}

#[cfg(windows)]
pub fn metadata(path: &Path) -> io::Result<Metadata> {
    windows::metadata(path)
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{
        fs::OpenOptions,
        mem::{size_of, zeroed},
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FileAttributeTagInfo, FileCompressionInfo, FileStandardInfo, GetFileInformationByHandle,
        GetFileInformationByHandleEx, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_COMPRESSED,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SPARSE_FILE,
        FILE_ATTRIBUTE_TAG_INFO, FILE_COMPRESSION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_INFO_BY_HANDLE_CLASS, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO,
    };

    // Name-surrogate reparse tags redirect paths (symlinks, junctions, mount points).
    // Other tags, such as cloud placeholders, do not by themselves redirect paths.
    const NAME_SURROGATE: u32 = 0x2000_0000;

    fn query<T>(file: &fs::File, class: FILE_INFO_BY_HANDLE_CLASS) -> io::Result<T> {
        // SAFETY: callers use the Windows struct matching `class`. All are plain
        // C data with valid zero initialization; the handle stays alive throughout.
        unsafe {
            let mut info: T = zeroed();
            if GetFileInformationByHandleEx(
                file.as_raw_handle(),
                class,
                (&mut info as *mut T).cast(),
                size_of::<T>() as u32,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(info)
        }
    }

    pub(super) fn metadata(path: &Path) -> io::Result<Metadata> {
        // Attribute access avoids reading/hydrating file contents. Open the
        // reparse point itself, including dangling links and directory junctions.
        let file = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        // SAFETY: info is a correctly sized writable Windows struct and file
        // owns the handle until after this call and the following queries.
        let info = unsafe {
            let mut info: BY_HANDLE_FILE_INFORMATION = zeroed();
            if GetFileInformationByHandle(file.as_raw_handle(), &mut info) == 0 {
                return Err(io::Error::last_os_error());
            }
            info
        };
        let attributes = info.dwFileAttributes;
        let link = if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            let tag: FILE_ATTRIBUTE_TAG_INFO = query(&file, FileAttributeTagInfo)?;
            tag.ReparseTag & NAME_SURROGATE != 0
        } else {
            false
        };
        let directory = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        let standard: FILE_STANDARD_INFO = query(&file, FileStandardInfo)?;
        let allocated = if !link
            && !directory
            && attributes & (FILE_ATTRIBUTE_COMPRESSED | FILE_ATTRIBUTE_SPARSE_FILE) != 0
        {
            let compression: FILE_COMPRESSION_INFO = query(&file, FileCompressionInfo)?;
            compression.CompressedFileSize
        } else {
            standard.AllocationSize
        };
        Ok(Metadata {
            identity: (
                u64::from(info.dwVolumeSerialNumber),
                (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            ),
            links: u64::from(info.nNumberOfLinks),
            apparent: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
            allocated: allocated.max(0) as u64,
            directory: directory && !link,
            link,
            directory_link: directory && link,
        })
    }
}

#[cfg(all(test, windows))]
pub mod test_support {
    use super::*;
    use std::{os::windows::io::AsRawHandle, process::Command};
    use windows_sys::Win32::System::{Ioctl::FSCTL_SET_SPARSE, IO::DeviceIoControl};

    pub fn junction(link: &Path, target: &Path) {
        let output = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "junction creation failed: {output:?}"
        );
    }

    pub fn sparse(path: &Path, length: u64) {
        let file = fs::File::create(path).unwrap();
        let mut returned = 0;
        // SAFETY: the file handle remains live, no input/output buffers are
        // required for FSCTL_SET_SPARSE, and returned points to writable storage.
        let success = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                FSCTL_SET_SPARSE,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(success, 0, "{}", io::Error::last_os_error());
        file.set_len(length).unwrap();
    }
}

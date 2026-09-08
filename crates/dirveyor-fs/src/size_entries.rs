//! Enumeration specialized for complete size walks: files need only their size,
//! and only ordinary directories need an allocated child path.
use std::{
    io,
    path::{Path, PathBuf},
};

pub(crate) enum SizeEntry {
    Directory(PathBuf),
    File(u64),
    Skipped,
}

#[cfg(not(windows))]
pub(crate) fn read_dir(
    path: &Path,
) -> io::Result<impl Iterator<Item = io::Result<SizeEntry>> + use<>> {
    Ok(std::fs::read_dir(path)?.map(|entry| {
        let entry = entry?;
        let kind = entry.file_type()?;
        Ok(if kind.is_symlink() {
            SizeEntry::Skipped
        } else if kind.is_dir() {
            SizeEntry::Directory(entry.path())
        } else if kind.is_file() {
            SizeEntry::File(entry.metadata()?.len())
        } else {
            SizeEntry::Skipped
        })
    }))
}

#[cfg(windows)]
pub(crate) use windows::read_dir;

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{
        ffi::OsString,
        os::windows::ffi::{OsStrExt, OsStringExt},
        ptr,
    };
    use windows_sys::Win32::{
        Foundation::{
            ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES,
            ERROR_NOT_SUPPORTED, HANDLE, INVALID_HANDLE_VALUE,
        },
        Storage::FileSystem::{
            FILE_ATTRIBUTE_DEVICE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
            FIND_FIRST_EX_LARGE_FETCH, FindClose, FindExInfoBasic, FindExSearchNameMatch,
            FindFirstFileExW, FindNextFileW, WIN32_FIND_DATAW,
        },
    };

    pub(crate) struct Entries {
        handle: HANDLE,
        directory: PathBuf,
        data: WIN32_FIND_DATAW,
        first: bool,
        done: bool,
    }

    impl Drop for Entries {
        fn drop(&mut self) {
            if self.handle != INVALID_HANDLE_VALUE {
                // SAFETY: this iterator owns the successful search handle.
                unsafe {
                    FindClose(self.handle);
                }
            }
        }
    }

    // Callers resolve the root once with fs::canonicalize. Its verbatim Windows
    // prefix is preserved on child paths, supporting UNC and paths > MAX_PATH.
    pub(crate) fn read_dir(path: &Path) -> io::Result<Entries> {
        let pattern: Vec<u16> = path
            .join("*")
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        // SAFETY: every bit pattern is valid for the integer fields and arrays.
        let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
        let mut flags = FIND_FIRST_EX_LARGE_FETCH;
        let handle = loop {
            // SAFETY: pattern is terminated and data is writable for the call.
            let handle = unsafe {
                FindFirstFileExW(
                    pattern.as_ptr(),
                    FindExInfoBasic,
                    (&mut data as *mut WIN32_FIND_DATAW).cast(),
                    FindExSearchNameMatch,
                    ptr::null(),
                    flags,
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                break handle;
            }
            let error = io::Error::last_os_error();
            match error.raw_os_error().map(|code| code as u32) {
                Some(ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED) if flags != 0 => {
                    flags = 0;
                }
                Some(ERROR_FILE_NOT_FOUND) => break INVALID_HANDLE_VALUE,
                _ => return Err(error),
            }
        };
        Ok(Entries {
            handle,
            directory: path.to_path_buf(),
            data,
            first: true,
            done: handle == INVALID_HANDLE_VALUE,
        })
    }

    impl Iterator for Entries {
        type Item = io::Result<SizeEntry>;
        fn next(&mut self) -> Option<Self::Item> {
            while !self.done {
                if self.first {
                    self.first = false;
                }
                // SAFETY: handle is live and data is writable for the call.
                else if unsafe { FindNextFileW(self.handle, &mut self.data) } == 0 {
                    let error = io::Error::last_os_error();
                    self.done = true;
                    return if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                        None
                    } else {
                        Some(Err(error))
                    };
                }
                let name = &self.data.cFileName;
                if name[0] == b'.' as u16
                    && (name[1] == 0 || (name[1] == b'.' as u16 && name[2] == 0))
                {
                    continue;
                }
                let attributes = self.data.dwFileAttributes;
                let entry =
                    if attributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DEVICE) != 0 {
                        SizeEntry::Skipped
                    } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                        let end = name
                            .iter()
                            .position(|unit| *unit == 0)
                            .unwrap_or(name.len());
                        SizeEntry::Directory(self.directory.join(OsString::from_wide(&name[..end])))
                    } else {
                        SizeEntry::File(
                            (u64::from(self.data.nFileSizeHigh) << 32)
                                | u64::from(self.data.nFileSizeLow),
                        )
                    };
                return Some(Ok(entry));
            }
            None
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // Exercise the first enumeration record without opening a real file or
        // allocating gigabytes on disk. next() reads the record before any API call.
        fn first_record(attributes: u32, high: u32, low: u32) -> SizeEntry {
            // SAFETY: the struct consists entirely of integers and arrays.
            let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
            data.cFileName[0] = b'x' as u16;
            data.dwFileAttributes = attributes;
            data.nFileSizeHigh = high;
            data.nFileSizeLow = low;
            Entries {
                handle: INVALID_HANDLE_VALUE,
                directory: PathBuf::from("unused"),
                data,
                first: true,
                done: false,
            }
            .next()
            .unwrap()
            .unwrap()
        }

        #[test]
        fn preserves_64_bit_file_sizes_and_empty_files() {
            assert!(
                matches!(first_record(0, 2, 7), SizeEntry::File(size) if size == (2_u64 << 32) + 7)
            );
            assert!(matches!(first_record(0, 0, 0), SizeEntry::File(0)));
        }

        #[test]
        fn skips_file_and_directory_reparse_points() {
            assert!(matches!(
                first_record(FILE_ATTRIBUTE_REPARSE_POINT, 0, 4),
                SizeEntry::Skipped
            ));
            assert!(matches!(
                first_record(
                    FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY,
                    0,
                    4
                ),
                SizeEntry::Skipped
            ));
        }
    }
}

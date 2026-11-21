use std::path::PathBuf;

pub(crate) fn documents_root() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Documents)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("USERPROFILE").map(|path| PathBuf::from(path).join("Documents"))
    }
}

pub(crate) fn public_documents_root() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_PublicDocuments)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("PUBLIC").map(|path| PathBuf::from(path).join("Documents"))
    }
}

#[cfg(windows)]
fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt, ptr};
    use windows_sys::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{SHGetKnownFolderPath, KF_FLAG_DONT_VERIFY},
    };

    let mut raw = ptr::null_mut();
    let status = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DONT_VERIFY as u32, ptr::null_mut(), &mut raw) };
    if raw.is_null() {
        return None;
    }
    let result = if status >= 0 {
        let mut length = 0;
        unsafe {
            while *raw.add(length) != 0 {
                length += 1;
            }
            let path = PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(raw, length)));
            path.is_absolute().then_some(path)
        }
    } else {
        None
    };
    unsafe { CoTaskMemFree(raw.cast()) };
    result
}

pub(crate) fn is_cloud_placeholder(path: &std::path::Path) -> bool {
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
    #[cfg(windows)]
    {
        use std::{fs::OpenOptions, mem::size_of, os::windows::io::AsRawHandle};
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::{
            Storage::FileSystem::{
                GetFileInformationByHandleEx, FileAttributeTagInfo, FILE_ATTRIBUTE_TAG_INFO,
                FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
            },
            System::SystemServices::{IO_REPARSE_TAG_CLOUD, IO_REPARSE_TAG_CLOUD_MASK},
        };

        let Ok(handle) = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path) else { return false };
        let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
        let read = unsafe {
            GetFileInformationByHandleEx(
                handle.as_raw_handle(),
                FileAttributeTagInfo,
                (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
                size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
            )
        };
        read != 0 && (info.ReparseTag & !IO_REPARSE_TAG_CLOUD_MASK) == IO_REPARSE_TAG_CLOUD
    }
}

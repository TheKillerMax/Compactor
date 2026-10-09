#![allow(non_camel_case_types, non_snake_case, dead_code)]

use std::convert::TryFrom;
use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde_derive::{Deserialize, Serialize};

use winapi::shared::minwindef::{BOOL, DWORD, PBOOL, PULONG, ULONG};
use winapi::shared::ntdef::PVOID;
use winapi::shared::winerror::{HRESULT_CODE, SUCCEEDED, WAIT_TIMEOUT};
use winapi::um::fileapi::{
    GetFileInformationByHandle, GetFinalPathNameByHandleW, BY_HANDLE_FILE_INFORMATION,
    FILE_COMPRESSION_INFO,
};
use winapi::um::handleapi::CloseHandle;
use winapi::um::ioapiset::DeviceIoControl;
use winapi::um::minwinbase::{FileAttributeTagInfo, FileCompressionInfo};
use winapi::um::synchapi::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use winapi::um::winbase::{
    GetFileInformationByHandleEx, FILE_FLAG_OPEN_REPARSE_POINT, WAIT_ABANDONED, WAIT_FAILED,
    WAIT_OBJECT_0,
};
use winapi::um::winioctl::{
    FSCTL_DELETE_EXTERNAL_BACKING, FSCTL_GET_EXTERNAL_BACKING, FSCTL_SET_EXTERNAL_BACKING,
};
use winapi::um::winnt::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_READ,
    FILE_WRITE_ATTRIBUTES, HANDLE, HRESULT, LPCWSTR,
};
use winapi::um::winver::{GetFileVersionInfoA, GetFileVersionInfoSizeA, VerQueryValueA};

// winapi 0.3.9 names the first FILE_ATTRIBUTE_TAG_INFO member
// NextEntryOffset by mistake. Define the documented two-DWORD layout locally
// so its meaning stays explicit when validating reparse tags.
#[repr(C)]
struct AttributeTagInfo {
    file_attributes: DWORD,
    reparse_tag: DWORD,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct _WOF_FILE_COMPRESSION_INFO_V1 {
    Algorithm: ULONG,
    Flags: ULONG,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct _WOF_EXTERNAL_INFO {
    Version: ULONG,
    Provider: ULONG,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct _FILE_PROVIDER_EXTERNAL_INFO_V1 {
    Version: ULONG,
    Algorithm: ULONG,
    Flags: ULONG,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct VS_FIXEDFILEINFO {
    dwSignature: DWORD,
    dwStrucVersion: DWORD,
    dwFileVersionMS: DWORD,
    dwFileVersionLS: DWORD,
    dwProductVersionMS: DWORD,
    dwProductVersionLS: DWORD,
    dwFileFlagsMask: DWORD,
    dwFileFlags: DWORD,
    dwFileOS: DWORD,
    dwFileType: DWORD,
    dwFileSubtype: DWORD,
    dwFileDateMS: DWORD,
    dwFileDateLS: DWORD,
}

const VS_FIXEDFILEINFO_SIGNATURE: DWORD = 0xFEEF_04BD;
const FILE_PROVIDER_COMPRESSION_XPRESS4K: ULONG = 0;
const FILE_PROVIDER_COMPRESSION_LZX: ULONG = 1;
const FILE_PROVIDER_COMPRESSION_XPRESS8K: ULONG = 2;
const FILE_PROVIDER_COMPRESSION_XPRESS16K: ULONG = 3;
const ERROR_OBJECT_NOT_EXTERNALLY_BACKED: i32 = 342;
const ERROR_COMPRESSION_NOT_BENEFICIAL: i32 = 344;
const IO_REPARSE_TAG_WOF: DWORD = 0x8000_0017;
const FILE_PROVIDER_CURRENT_VERSION: ULONG = 1;
const WOF_CURRENT_VERSION: ULONG = 1;
const WOF_PROVIDER_FILE: ULONG = 2;

impl Default for _FILE_PROVIDER_EXTERNAL_INFO_V1 {
    fn default() -> Self {
        Self::from(Compression::default())
    }
}

impl Default for _WOF_EXTERNAL_INFO {
    fn default() -> Self {
        Self {
            Version: WOF_CURRENT_VERSION,
            Provider: WOF_PROVIDER_FILE,
        }
    }
}

impl From<Compression> for _FILE_PROVIDER_EXTERNAL_INFO_V1 {
    fn from(compression: Compression) -> Self {
        Self {
            Version: FILE_PROVIDER_CURRENT_VERSION,
            Algorithm: compression.into(),
            Flags: 0,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Compression {
    Xpress4k,
    Xpress8k,
    Xpress16k,
    Lzx,
}

impl Default for Compression {
    fn default() -> Self {
        Compression::Lzx
    }
}

impl std::fmt::Display for Compression {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match self {
            Compression::Xpress4k => "XPRESS4K",
            Compression::Xpress8k => "XPRESS8K",
            Compression::Xpress16k => "XPRESS16K",
            Compression::Lzx => "LZX",
        })
    }
}

impl FromStr for Compression {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_uppercase().as_str() {
            "XPRESS4K" => Ok(Compression::Xpress4k),
            "XPRESS8K" => Ok(Compression::Xpress8k),
            "XPRESS16K" => Ok(Compression::Xpress16k),
            "LZX" => Ok(Compression::Lzx),
            _ => Err(()),
        }
    }
}

impl TryFrom<ULONG> for Compression {
    type Error = ();

    fn try_from(value: ULONG) -> Result<Self, Self::Error> {
        match value {
            FILE_PROVIDER_COMPRESSION_XPRESS4K => Ok(Compression::Xpress4k),
            FILE_PROVIDER_COMPRESSION_XPRESS8K => Ok(Compression::Xpress8k),
            FILE_PROVIDER_COMPRESSION_XPRESS16K => Ok(Compression::Xpress16k),
            FILE_PROVIDER_COMPRESSION_LZX => Ok(Compression::Lzx),
            _ => Err(()),
        }
    }
}

impl From<Compression> for ULONG {
    fn from(value: Compression) -> Self {
        match value {
            Compression::Xpress4k => FILE_PROVIDER_COMPRESSION_XPRESS4K,
            Compression::Xpress8k => FILE_PROVIDER_COMPRESSION_XPRESS8K,
            Compression::Xpress16k => FILE_PROVIDER_COMPRESSION_XPRESS16K,
            Compression::Lzx => FILE_PROVIDER_COMPRESSION_LZX,
        }
    }
}

/// Unknown providers, versions and reparse tags must never be treated as plain
/// data: WIM-backed files, links and future WOF formats are outside our scope.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum BackingState {
    None,
    Wof(Compression),
    Unsupported,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct FileIdentity {
    pub volume_serial: DWORD,
    pub file_index: u64,
    pub links: DWORD,
}

/// Cooperative exclusion for Compactor instances across Windows sessions.
/// File sharing alone does not exclude handles that request WRITE_ATTRIBUTES,
/// which is sufficient for another WOF operation. The raw HANDLE also makes
/// this guard non-Send: mutex ownership must be released on the acquiring thread.
#[derive(Debug)]
pub struct WofIdentityLock(HANDLE);

impl Drop for WofIdentityLock {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

pub fn lock_wof_identity(identity: &FileIdentity) -> io::Result<WofIdentityLock> {
    // Global named mutexes do not require SeCreateGlobalPrivilege; that check
    // applies to creating global file-mapping and symbolic-link objects.
    let name = format!(
        r"Global\Compactor.WOF.{:08x}.{:016x}",
        identity.volume_serial, identity.file_index
    );
    let name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mutex = unsafe { CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr()) };
    if mutex.is_null() {
        return Err(io::Error::last_os_error());
    }
    match unsafe { WaitForSingleObject(mutex, 0) } {
        WAIT_OBJECT_0 | WAIT_ABANDONED => {
            // Abandonment transfers ownership. The caller always re-queries
            // current backing and metadata after acquiring this guard.
            Ok(WofIdentityLock(mutex))
        }
        status => {
            let error = if status == WAIT_TIMEOUT {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "Another Compactor operation is already working on this file",
                )
            } else if status == WAIT_FAILED {
                io::Error::last_os_error()
            } else {
                io::Error::new(
                    io::ErrorKind::Other,
                    "Unexpected result while acquiring the file operation mutex",
                )
            };
            unsafe {
                let _ = CloseHandle(mutex);
            }
            Err(error)
        }
    }
}

pub fn file_identity_handle(file: &std::fs::File) -> io::Result<FileIdentity> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ret = unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) };
    if ret == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileIdentity {
        volume_serial: info.dwVolumeSerialNumber,
        file_index: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
        links: info.nNumberOfLinks,
    })
}

pub fn system_supports_compression() -> io::Result<bool> {
    let dll = CString::new("WofUtil.dll").unwrap();
    let path = CString::new("\\").unwrap();
    let mut handle = 0;
    let len = unsafe { GetFileVersionInfoSizeA(dll.as_ptr(), &mut handle) };
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = vec![0u8; len as usize];
    let ret = unsafe { GetFileVersionInfoA(dll.as_ptr(), handle, len, buf.as_mut_ptr() as PVOID) };
    if ret == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut pinfo: PVOID = std::ptr::null_mut();
    let mut pinfo_size = 0;
    let ret = unsafe {
        VerQueryValueA(
            buf.as_mut_ptr() as PVOID,
            path.as_ptr(),
            &mut pinfo,
            &mut pinfo_size,
        )
    };
    if ret == 0 {
        return Err(io::Error::last_os_error());
    }
    if pinfo.is_null() || (pinfo_size as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid WofUtil version information",
        ));
    }
    // VerQueryValue returns storage inside a byte buffer; that buffer does not
    // promise the alignment required for a Rust reference to VS_FIXEDFILEINFO.
    let pinfo = unsafe { std::ptr::read_unaligned(pinfo as *const VS_FIXEDFILEINFO) };
    if pinfo.dwSignature != VS_FIXEDFILEINFO_SIGNATURE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid WofUtil version signature",
        ));
    }
    Ok((pinfo.dwFileVersionMS >> 16) & 0xffff >= 10)
}

pub fn file_supports_compression<P: AsRef<Path>>(path: P) -> io::Result<bool> {
    let file = std::fs::File::open(path)?;
    let mut version: ULONG = 0;
    let ret = unsafe {
        WofGetDriverVersion(
            file.as_raw_handle() as HANDLE,
            WOF_PROVIDER_FILE,
            &mut version,
        )
    };
    if SUCCEEDED(ret) {
        Ok(version > 0)
    } else if HRESULT_CODE(ret) == 1 {
        // WofGetDriverVersion documents ERROR_INVALID_FUNCTION when no WOF
        // driver is attached for this volume/provider.
        Ok(false)
    } else {
        Err(io::Error::from_raw_os_error(HRESULT_CODE(ret)))
    }
}

fn decode_external_backing(data: &[u8]) -> io::Result<BackingState> {
    if data.len() < 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Truncated WOF backing information",
        ));
    }
    let field = |offset| {
        ULONG::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ])
    };
    if field(0) != WOF_CURRENT_VERSION || field(4) != WOF_PROVIDER_FILE {
        return Ok(BackingState::Unsupported);
    }
    if data.len() < 20 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Truncated WOF file-provider information",
        ));
    }
    if field(8) != FILE_PROVIDER_CURRENT_VERSION || field(16) != 0 {
        return Ok(BackingState::Unsupported);
    }
    Ok(match Compression::try_from(field(12)) {
        Ok(compression) => BackingState::Wof(compression),
        Err(_) => BackingState::Unsupported,
    })
}

pub fn detect_backing_handle(file: &std::fs::File) -> io::Result<BackingState> {
    let mut tag: AttributeTagInfo = unsafe { std::mem::zeroed() };
    let ret = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileAttributeTagInfo,
            &mut tag as *mut _ as PVOID,
            std::mem::size_of::<AttributeTagInfo>() as DWORD,
        )
    };
    if ret == 0 {
        return Err(io::Error::last_os_error());
    }
    if tag.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        && tag.reparse_tag != IO_REPARSE_TAG_WOF
    {
        return Ok(BackingState::Unsupported);
    }

    // Large enough for the documented WIM and file-provider structures. Parse
    // byte fields explicitly instead of assuming the output is aligned.
    let mut data = [0u8; 4096];
    let mut returned: DWORD = 0;
    let ret = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as HANDLE,
            FSCTL_GET_EXTERNAL_BACKING,
            std::ptr::null_mut(),
            0,
            data.as_mut_ptr() as PVOID,
            data.len() as DWORD,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ret == 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(ERROR_OBJECT_NOT_EXTERNALLY_BACKED) {
            // An unexplained WOF reparse point is not a safe plain-file target.
            return Ok(if tag.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                BackingState::None
            } else {
                BackingState::Unsupported
            });
        }
        return Err(err);
    }
    if returned as usize > data.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid WOF response length",
        ));
    }
    decode_external_backing(&data[..returned as usize])
}

pub fn detect_compression<P: AsRef<OsStr>>(path: P) -> io::Result<Option<Compression>> {
    let mut p: Vec<u16> = path.as_ref().encode_wide().collect();
    p.push(0);
    let mut is_external: BOOL = 0;
    let mut provider: ULONG = 0;
    let mut file_info: _WOF_FILE_COMPRESSION_INFO_V1 = unsafe { std::mem::zeroed() };
    let expected = std::mem::size_of::<_WOF_FILE_COMPRESSION_INFO_V1>() as ULONG;
    let mut len = expected;
    let ret = unsafe {
        WofIsExternalFile(
            p.as_ptr(),
            &mut is_external,
            &mut provider,
            &mut file_info as *mut _ as PVOID,
            &mut len,
        )
    };
    if !SUCCEEDED(ret) {
        return Err(io::Error::from_raw_os_error(HRESULT_CODE(ret)));
    }
    if is_external == 0 || provider != WOF_PROVIDER_FILE {
        return Ok(None);
    }
    // WofIsExternalFile can return S_OK for an insufficient buffer without
    // populating it. Never interpret the initial zeroes as XPRESS4K.
    if len != expected || file_info.Flags != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Unsupported WOF compression information",
        ));
    }
    Compression::try_from(file_info.Algorithm)
        .map(Some)
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Unknown WOF compression algorithm",
            )
        })
}

pub fn final_path_handle(file: &std::fs::File) -> io::Result<PathBuf> {
    let mut buffer = vec![0u16; 512];
    loop {
        // Flags 0 request a normalized DOS-volume path, normally with a \\?\
        // prefix. Keep that prefix to support long names when reopening.
        let len = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle() as HANDLE,
                buffer.as_mut_ptr(),
                buffer.len() as DWORD,
                0,
            )
        };
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        if (len as usize) < buffer.len() {
            return Ok(PathBuf::from(OsString::from_wide(&buffer[..len as usize])));
        }
        if len > 32_768 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid final path length",
            ));
        }
        buffer.resize(len as usize + 1, 0);
    }
}

/// Reopen the normal WOF view only after its backing was identified. The caller
/// keeps the original share-denying handle open; file identity is checked even
/// if an ancestor was renamed between resolving and opening the final path.
pub fn reopen_wof_handle(file: &std::fs::File) -> io::Result<std::fs::File> {
    if !matches!(detect_backing_handle(file)?, BackingState::Wof(_)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "The file is not a supported WOF file",
        ));
    }
    let before = file_identity_handle(file)?;
    let reopened = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_DATA | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ)
        .open(final_path_handle(file)?)?;
    let after = file_identity_handle(&reopened)?;
    if before.volume_serial != after.volume_serial || before.file_index != after.file_index {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "The file identity changed while reopening the WOF view",
        ));
    }
    Ok(reopened)
}

fn query_allocated_size(file: &std::fs::File) -> io::Result<u64> {
    let mut info: FILE_COMPRESSION_INFO = unsafe { std::mem::zeroed() };
    let ret = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileCompressionInfo,
            &mut info as *mut _ as PVOID,
            std::mem::size_of::<FILE_COMPRESSION_INFO>() as DWORD,
        )
    };
    if ret == 0 {
        return Err(io::Error::last_os_error());
    }
    let size = unsafe { *info.CompressedFileSize.QuadPart() };
    if size < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned a negative allocated file size",
        ));
    }
    Ok(size as u64)
}

/// Query FileCompressionInfo on the normal file-system view. AllocationSize on
/// the underlying sparse stream is not a substitute for WOF storage usage, and
/// its hidden backing stream must not be added a second time.
pub fn allocated_size_handle(file: &std::fs::File) -> io::Result<u64> {
    if matches!(detect_backing_handle(file)?, BackingState::Wof(_)) {
        let view = reopen_wof_handle(file)?;
        let size = query_allocated_size(&view)?;
        if size == 0 && view.metadata()?.len() > 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows did not expose a measurable WOF allocation",
            ));
        }
        Ok(size)
    } else {
        query_allocated_size(file)
    }
}

unsafe fn as_byte_slice<T: Sized + Copy>(p: &T) -> &[u8] {
    std::slice::from_raw_parts((p as *const T) as *const u8, std::mem::size_of::<T>())
}

pub fn open_for_wof<P: AsRef<Path>>(path: P) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .access_mode(FILE_READ_DATA | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

pub fn compress_file<P: AsRef<Path>>(path: P, compression: Compression) -> io::Result<bool> {
    let file = open_for_wof(path)?;
    compress_file_handle(&file, compression)
}

pub fn compress_file_handle(file: &std::fs::File, compression: Compression) -> io::Result<bool> {
    if detect_backing_handle(file)? != BackingState::None {
        // Converting an existing WOF/WIM representation is intentionally not an
        // implicit side effect of a compression request.
        return Ok(false);
    }
    const LEN: usize = std::mem::size_of::<_WOF_EXTERNAL_INFO>()
        + std::mem::size_of::<_FILE_PROVIDER_EXTERNAL_INFO_V1>();
    let mut data = [0u8; LEN];
    let (wof, inf) = data.split_at_mut(std::mem::size_of::<_WOF_EXTERNAL_INFO>());
    unsafe {
        wof.copy_from_slice(as_byte_slice(&_WOF_EXTERNAL_INFO::default()));
        inf.copy_from_slice(as_byte_slice(&_FILE_PROVIDER_EXTERNAL_INFO_V1::from(
            compression,
        )));
    }
    let mut bytes_returned: DWORD = 0;
    let ret = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as HANDLE,
            FSCTL_SET_EXTERNAL_BACKING,
            data.as_mut_ptr() as PVOID,
            data.len() as DWORD,
            std::ptr::null_mut(),
            0,
            &mut bytes_returned,
            std::ptr::null_mut(),
        )
    };
    // DeviceIoControl returns a Win32 BOOL, not an HRESULT.
    if ret != 0 {
        Ok(true)
    } else {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(ERROR_COMPRESSION_NOT_BENEFICIAL) {
            Ok(false)
        } else {
            Err(err)
        }
    }
}

pub fn uncompress_file<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let file = open_for_wof(path)?;
    uncompress_file_handle(&file)
}

pub fn uncompress_file_handle(file: &std::fs::File) -> io::Result<()> {
    match detect_backing_handle(file)? {
        BackingState::None => return Ok(()),
        BackingState::Unsupported => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Unsupported external backing will not be removed",
            ));
        }
        BackingState::Wof(_) => {}
    }
    let view = reopen_wof_handle(file)?;
    let mut bytes_returned: DWORD = 0;
    let ret = unsafe {
        DeviceIoControl(
            view.as_raw_handle() as HANDLE,
            FSCTL_DELETE_EXTERNAL_BACKING,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            0,
            &mut bytes_returned,
            std::ptr::null_mut(),
        )
    };
    if ret != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

// The Windows SDK uses WofUtil.lib; MinGW installs libwofutil.a. On a
// case-sensitive cross-build host the library spelling must match exactly.
#[cfg_attr(target_env = "msvc", link(name = "WofUtil"))]
#[cfg_attr(target_env = "gnu", link(name = "wofutil"))]
extern "system" {
    pub fn WofGetDriverVersion(
        file_or_volume_handle: HANDLE,
        provider: ULONG,
        version: PULONG,
    ) -> HRESULT;
    pub fn WofIsExternalFile(
        file_path: LPCWSTR,
        is_external_file: PBOOL,
        provider: PULONG,
        external_file_info: PVOID,
        length: PULONG,
    ) -> HRESULT;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background::{Background, ControlToken};
    use crate::config::Config;
    use crate::folder::FolderScan;
    use filesize::PathExt;

    fn backing_data(
        version: u32,
        provider: u32,
        file_version: u32,
        algorithm: u32,
        flags: u32,
    ) -> Vec<u8> {
        [version, provider, file_version, algorithm, flags]
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    #[test]
    fn algorithms_round_trip_ui_and_windows_values() {
        for algorithm in [
            Compression::Xpress4k,
            Compression::Xpress8k,
            Compression::Xpress16k,
            Compression::Lzx,
        ] {
            assert_eq!(Ok(algorithm), algorithm.to_string().parse());
            assert_eq!(Ok(algorithm), algorithm.to_string().to_lowercase().parse());
            assert_eq!(Ok(algorithm), Compression::try_from(ULONG::from(algorithm)));
        }
        assert!("LZMS".parse::<Compression>().is_err());
        assert!(Compression::try_from(4).is_err());
    }

    #[test]
    fn backing_parser_rejects_truncated_and_unknown_structures() {
        let valid = backing_data(1, 2, 1, 1, 0);
        assert_eq!(
            BackingState::Wof(Compression::Lzx),
            decode_external_backing(&valid).unwrap()
        );
        for len in 0..20 {
            assert!(decode_external_backing(&valid[..len]).is_err());
        }
        for unsupported in [
            backing_data(2, 2, 1, 1, 0),
            backing_data(1, 1, 1, 1, 0),
            backing_data(1, 2, 2, 1, 0),
            backing_data(1, 2, 1, 99, 0),
            backing_data(1, 2, 1, 1, 1),
        ] {
            assert_eq!(
                BackingState::Unsupported,
                decode_external_backing(&unsupported).unwrap()
            );
        }
    }

    #[test]
    fn identity_lock_excludes_other_threads_and_releases_on_drop() {
        let dir = tempdir::TempDir::new("compactor-file-mutex").unwrap();
        let path = dir.path().join("fixture.bin");
        std::fs::write(&path, b"mutex fixture").unwrap();
        let file = open_for_wof(&path).unwrap();
        let identity = file_identity_handle(&file).unwrap();
        let guard = lock_wof_identity(&identity).unwrap();
        // Named Windows mutexes are reentrant for the same thread. Compete
        // from another thread so this exercises actual mutual exclusion.
        let blocked = std::thread::spawn(move || lock_wof_identity(&identity).map(|_| ()))
            .join()
            .unwrap()
            .unwrap_err();
        assert_eq!(io::ErrorKind::WouldBlock, blocked.kind());
        drop(guard);
        std::thread::spawn(move || lock_wof_identity(&identity).map(|_| ()))
            .join()
            .unwrap()
            .unwrap();
    }

    #[test]
    #[ignore = "Requires Windows 10+ and a local NTFS temporary directory; run with --ignored"]
    fn wof_round_trip_preserves_bytes_and_reports_real_storage_for_all_algorithms() {
        assert!(system_supports_compression().expect("WOF version probe"));
        let dir = tempdir::TempDir::new("compactor-wof-round-trip").unwrap();
        let data =
            b"Compactor WOF integration fixture: verify every decompressed byte.\r\n".repeat(32768);
        for algorithm in [
            Compression::Xpress4k,
            Compression::Xpress8k,
            Compression::Xpress16k,
            Compression::Lzx,
        ] {
            let path = dir.path().join(format!("{}.bin", algorithm));
            std::fs::write(&path, &data).unwrap();
            assert!(file_supports_compression(&path).expect("NTFS WOF probe"));
            let guard = open_for_wof(&path).unwrap();
            let before = allocated_size_handle(&guard).unwrap();
            assert_eq!(BackingState::None, detect_backing_handle(&guard).unwrap());
            assert!(compress_file_handle(&guard, algorithm).unwrap());
            assert_eq!(
                BackingState::Wof(algorithm),
                detect_backing_handle(&guard).unwrap()
            );
            assert_eq!(
                Some(algorithm),
                detect_compression(path.as_os_str()).unwrap()
            );
            let compressed = allocated_size_handle(&guard).unwrap();
            assert!(compressed > 0 && compressed < before);
            assert_eq!(compressed, path.size_on_disk().unwrap());
            assert_eq!(data, std::fs::read(&path).unwrap());
            // Regression: native WOF sparse/reparse attributes must remain
            // visible to the scanner as decompressible WOF files.
            let scanned = FolderScan::new(dir.path(), &Config::default())
                .unwrap()
                .run(&ControlToken::new())
                .unwrap();
            assert_eq!(1, scanned.compressed.count);
            assert_eq!(Some(algorithm), scanned.compressed.files[0].compression);
            // A second request must preserve the representation already in use.
            assert!(!compress_file_handle(&guard, Compression::Lzx).unwrap());
            assert_eq!(
                BackingState::Wof(algorithm),
                detect_backing_handle(&guard).unwrap()
            );
            uncompress_file_handle(&guard).unwrap();
            assert_eq!(BackingState::None, detect_backing_handle(&guard).unwrap());
            assert_eq!(data, std::fs::read(&path).unwrap());
            assert_eq!(before, allocated_size_handle(&guard).unwrap());
        }
    }
}

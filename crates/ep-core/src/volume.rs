//! NTFS 卷访问层：MFT 全量枚举（FSCTL_ENUM_USN_DATA）+ USN Journal 增量读取。
//! 需要管理员权限（打开卷设备 `\\.\C:` 要求管理员）。

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    FSCTL_CREATE_USN_JOURNAL, FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL,
    FSCTL_READ_USN_JOURNAL,
};

pub const GENERIC_READ: u32 = 0x8000_0000;
pub const DRIVE_FIXED: u32 = 3;
/// USN_RECORD_V2 最小长度（不含文件名）
const USN_RECORD_V2_MIN: usize = 60;

// USN_REASON_*（只取我们关心的）
pub const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
pub const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
pub const USN_REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
pub const USN_REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
pub const USN_REASON_BASIC_INFO_CHANGE: u32 = 0x0000_8000;
pub const USN_REASON_CLOSE: u32 = 0x8000_0000;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;

/// FSCTL_QUERY_USN_JOURNAL 输出（V1）
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct UsnJournalData {
    pub usn_journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub lowest_valid_usn: i64,
    pub max_usn: i64,
    pub maximum_size: u64,
    pub allocation_delta: u64,
    pub min_major_version: u16,
    pub max_major_version: u16,
}

/// FSCTL_ENUM_USN_DATA 输入（V0：只枚举 V2 记录）
#[repr(C)]
struct MftEnumDataV0 {
    start_file_reference_number: u64,
    low_usn: i64,
    high_usn: i64,
}

/// FSCTL_READ_USN_JOURNAL 输入（V1）
#[repr(C)]
struct ReadUsnJournalDataV1 {
    start_usn: i64,
    reason_mask: u32,
    return_only_on_close: u32,
    timeout: u64,
    bytes_to_wait_for: u64,
    usn_journal_id: u64,
    min_major_version: u16,
    max_major_version: u16,
}

/// FSCTL_CREATE_USN_JOURNAL 输入
#[repr(C)]
struct CreateUsnJournalData {
    maximum_size: u64,
    allocation_delta: u64,
}

/// USN_RECORD_V2（布局与 Win32 完全一致，FileName 起始于 file_name_offset）
#[repr(C)]
struct UsnRecordV2 {
    record_length: u32,
    _major: u16,
    _minor: u16,
    file_reference_number: u64,
    parent_file_reference_number: u64,
    _usn: i64,
    timestamp: i64,
    reason: u32,
    _source_info: u32,
    _security_id: u32,
    file_attributes: u32,
    file_name_length: u16,
    file_name_offset: u16,
}

/// 一条 USN 记录的解析结果
pub struct RawRecord {
    pub frn: u64,
    pub parent_frn: u64,
    pub name: String,
    pub attrs: u32,
    pub ts_unix: i64,
    pub reason: u32,
}

pub struct Volume {
    pub letter: char,
    handle: *mut core::ffi::c_void,
}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// 列出所有固定的 NTFS 盘符
pub fn fixed_ntfs_volumes() -> Vec<char> {
    let mut out = Vec::new();
    unsafe {
        let mask = GetLogicalDrives();
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            let root = format!("{letter}:\\");
            let wroot = wide(&root);
            if GetDriveTypeW(wroot.as_ptr()) != DRIVE_FIXED {
                continue;
            }
            let mut fs_name = [0u16; 32];
            let mut fs_flags = 0u32;
            let ok = GetVolumeInformationW(
                wroot.as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut fs_flags,
                fs_name.as_mut_ptr(),
                32,
            );
            if ok != 0 {
                let fs = String::from_utf16_lossy(
                    &fs_name[..fs_name.iter().position(|&c| c == 0).unwrap_or(32)],
                )
                .eq_ignore_ascii_case("ntfs");
                if fs {
                    out.push(letter);
                }
            }
        }
    }
    out
}

impl Volume {
    /// 调试用：原始卷句柄
    pub fn raw_handle(&self) -> isize {
        self.handle as isize
    }

    pub fn open(letter: char) -> io::Result<Volume> {
        let path = format!(r"\\.\{letter}:");
        unsafe {
            let handle = CreateFileW(
                wide(&path).as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            );
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            Ok(Volume { letter, handle })
        }
    }

    /// 确保 USN Journal 存在（不存在则创建，已存在则无操作）
    pub fn ensure_journal(&self) -> io::Result<()> {
        let input = CreateUsnJournalData { maximum_size: 0, allocation_delta: 0 };
        let mut ret = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                FSCTL_CREATE_USN_JOURNAL,
                &input as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<CreateUsnJournalData>() as u32,
                std::ptr::null_mut(),
                0,
                &mut ret,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn query_journal(&self) -> io::Result<UsnJournalData> {
        let mut out = UsnJournalData {
            usn_journal_id: 0, first_usn: 0, next_usn: 0, lowest_valid_usn: 0,
            max_usn: 0, maximum_size: 0, allocation_delta: 0,
            min_major_version: 0, max_major_version: 0,
        };
        let mut ret = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                FSCTL_QUERY_USN_JOURNAL,
                std::ptr::null(),
                0,
                &mut out as *mut _ as *mut core::ffi::c_void,
                std::mem::size_of::<UsnJournalData>() as u32,
                &mut ret,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(out)
        }
    }

    /// 全量枚举卷上所有文件记录（MFT 扫描）。每条记录回调 `f`。
    /// 返回记录总数。同一 FRN 的多条记录（硬链接/多流名）只回调第一条。
    pub fn enumerate_all<F: FnMut(RawRecord)>(&self, mut f: F) -> io::Result<u64> {
        let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut med = MftEnumDataV0 {
            start_file_reference_number: 0,
            low_usn: 0,
            high_usn: i64::MAX,
        };
        let mut buf = vec![0u8; 64 * 1024];
        let mut total: u64 = 0;
        loop {
            let mut ret = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    self.handle,
                    FSCTL_ENUM_USN_DATA,
                    &mut med as *mut _ as *const core::ffi::c_void,
                    std::mem::size_of::<MftEnumDataV0>() as u32,
                    buf.as_mut_ptr() as *mut core::ffi::c_void,
                    buf.len() as u32,
                    &mut ret,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                let err = io::Error::last_os_error();
                // ERROR_HANDLE_EOF = 38：枚举结束
                if err.raw_os_error() == Some(38) {
                    break;
                }
                return Err(err);
            }
            if ret as usize <= std::mem::size_of::<i64>() {
                break;
            }
            // 缓冲区头 8 字节是下一次枚举的游标（NextUsn / StartFRN）
            med.start_file_reference_number =
                u64::from_ne_bytes(buf[0..8].try_into().unwrap());
            let mut off = 8usize;
            let end = ret as usize;
            while off + USN_RECORD_V2_MIN <= end {
                let rec: &UsnRecordV2 = unsafe { &*(buf.as_ptr().add(off) as *const UsnRecordV2) };
                if rec.record_length == 0 {
                    break;
                }
                let name_off = rec.file_name_offset as usize;
                let name_len = rec.file_name_length as usize;
                if off + name_off + name_len <= end {
                    let name_u16: Vec<u16> = unsafe {
                        std::slice::from_raw_parts(
                            buf.as_ptr().add(off + name_off) as *const u16,
                            name_len / 2,
                        )
                    }
                    .to_vec();
                    let name = String::from_utf16_lossy(&name_u16);
                    if !name.is_empty() && seen.insert(rec.file_reference_number) {
                        f(RawRecord {
                            frn: rec.file_reference_number,
                            parent_frn: rec.parent_file_reference_number,
                            name,
                            attrs: rec.file_attributes,
                            ts_unix: filetime_to_unix(rec.timestamp),
                            reason: 0,
                        });
                        total += 1;
                    }
                }
                off += rec.record_length as usize;
            }
        }
        Ok(total)
    }

    /// 从 start_usn 开始读取 journal 增量。返回 (next_usn, records)。
    /// 空记录表示暂时无变更。start_usn 必须是合法记录边界（上次返回的 next_usn）。
    /// 注意：max_major_version=3 时系统返回 V3 记录，这里固定 2 以统一解析 V2。
    pub fn read_journal(&self, journal_id: u64, start_usn: i64) -> io::Result<(i64, Vec<RawRecord>)> {
        let input = ReadUsnJournalDataV1 {
            start_usn,
            reason_mask: 0xFFFF_FFFF,
            return_only_on_close: 0,
            timeout: 0,
            bytes_to_wait_for: 0,
            usn_journal_id: journal_id,
            min_major_version: 2,
            max_major_version: 2,
        };
        let mut buf = vec![0u8; 64 * 1024];
        let mut ret = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                FSCTL_READ_USN_JOURNAL,
                &input as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<ReadUsnJournalDataV1>() as u32,
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                buf.len() as u32,
                &mut ret,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if (ret as usize) <= 8 {
            return Ok((start_usn, Vec::new()));
        }
        let next_usn = i64::from_ne_bytes(buf[0..8].try_into().unwrap());
        let mut records = Vec::new();
        let mut off = 8usize;
        let end = ret as usize;
        while off + USN_RECORD_V2_MIN <= end {
            let head = &buf[off..];
            let record_length = u32::from_ne_bytes(head[0..4].try_into().unwrap()) as usize;
            let major = u16::from_ne_bytes(head[4..6].try_into().unwrap());
            if record_length == 0 {
                break;
            }
            if off + record_length <= end && major == 2 {
                let rec: &UsnRecordV2 = unsafe { &*(buf.as_ptr().add(off) as *const UsnRecordV2) };
                let name_off = rec.file_name_offset as usize;
                let name_len = rec.file_name_length as usize;
                if off + name_off + name_len <= end {
                    let name_u16: Vec<u16> = unsafe {
                        std::slice::from_raw_parts(
                            buf.as_ptr().add(off + name_off) as *const u16,
                            name_len / 2,
                        )
                    }
                    .to_vec();
                    records.push(RawRecord {
                        frn: rec.file_reference_number,
                        parent_frn: rec.parent_file_reference_number,
                        name: String::from_utf16_lossy(&name_u16),
                        attrs: rec.file_attributes,
                        ts_unix: filetime_to_unix(rec.timestamp),
                        reason: rec.reason,
                    });
                }
            }
            off += record_length;
        }
        Ok((next_usn, records))
    }
}

impl Drop for Volume {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

fn filetime_to_unix(ft: i64) -> i64 {
    let t = (ft - 116_444_736_000_000_000) / 10_000_000;
    // MFT 枚举记录的 TimeStamp 常为 0，归一为 0（UI 显示为空）
    if t < 0 { 0 } else { t }
}

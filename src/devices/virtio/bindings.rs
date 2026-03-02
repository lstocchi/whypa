pub const LINUX_EACCES: i32 = 13;
pub const LINUX_ENODATA: i32 = 61;
pub const LINUX_ENOSYS: i32 = 38;
pub const LINUX_ENOTEMPTY: i32 = 39;

pub const LINUX_O_APPEND: i32 = 1024;
pub const LINUX_O_CLOEXEC: i32 = 0x80000;
pub const LINUX_O_DIRECT: i32 = 0x4000;
pub const LINUX_O_DIRECTORY: i32 = 0x10000;
pub const LINUX_O_LARGEFILE: i32 = 0;
pub const LINUX_O_NOFOLLOW: i32 = 0x20000;
pub const LINUX_O_CREAT: i32 = 64;
pub const LINUX_O_EXCL: i32 = 128;
pub const LINUX_O_NOCTTY: i32 = 256;
pub const LINUX_O_NONBLOCK: i32 = 2048;
pub const LINUX_O_SYNC: i32 = 1052672;
pub const LINUX_O_TRUNC: i32 = 512;
pub const LINUX_O_RSYNC: i32 = 1052672;
pub const LINUX_O_DSYNC: i32 = 4096;
pub const LINUX_O_ASYNC: i32 = 0x2000;

pub const LINUX_RENAME_NOREPLACE: i32 = 1 << 0;
pub const LINUX_RENAME_EXCHANGE: i32 = 1 << 1;
pub const LINUX_RENAME_WHITEOUT: i32 = 1 << 2;

pub const LINUX_XATTR_CREATE: i32 = 1;
pub const LINUX_XATTR_REPLACE: i32 = 2;

pub fn win_to_linux_errno(win_err: u32) -> i32 {
    match win_err {
        0 => 0,
        // Basic Permissions & Existence
        1 | 1314 => 1,             // ERROR_INVALID_FUNCTION / PRIVILEGE_NOT_HELD -> EPERM
        2 | 3 => 2,                // ERROR_FILE_NOT_FOUND / PATH_NOT_FOUND -> ENOENT
        5 => LINUX_EACCES,         // ERROR_ACCESS_DENIED -> EACCES (13)
        
        // I/O & Hardware
        5 | 6 | 21 => 6,           // ERROR_INVALID_HANDLE / NOT_READY -> ENXIO
        1117 => 5,                 // ERROR_IO_DEVICE -> EIO
        112 => 28,                 // ERROR_DISK_FULL -> ENOSPC
        
        // Resource limits
        4 => 24,                   // ERROR_TOO_MANY_OPEN_FILES -> EMFILE
        8 | 14 => 12,              // ERROR_NOT_ENOUGH_MEMORY / OUTOFMEMORY -> ENOMEM
        
        // Logic & State
        80 | 183 => 17,            // ERROR_FILE_EXISTS / ALREADY_EXISTS -> EEXIST
        87 | 161 => 22,            // ERROR_INVALID_PARAMETER -> EINVAL
        145 => LINUX_ENOTEMPTY,    // ERROR_DIR_NOT_EMPTY -> ENOTEMPTY (39)
        120 => LINUX_ENOSYS,       // ERROR_CALL_NOT_IMPLEMENTED -> ENOSYS (38)
        
        // File System Specific
        4331 => LINUX_ENODATA,     // ERROR_NOT_FOUND (Extended Attributes) -> ENODATA (61)
        
        // Default
        _ => 5,                    // Default to EIO
    }
}
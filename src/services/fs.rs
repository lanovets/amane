use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::Duration;

use crate::Service;

#[derive(Default)]
pub struct Fs;

impl Service for Fs {
    fn new() -> Self {
        Self
    }

    fn interval() -> Duration {
        Duration::from_secs(60)
    }

    fn listen() {}
}

impl Fs {
    // bytes available to an unprivileged user on the filesystem holding `path`
    pub fn available(path: &Path) -> Option<u64> {
        let path = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
            return None;
        }
        Some(stat.f_bavail as u64 * stat.f_frsize as u64)
    }
}

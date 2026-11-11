//! Verification reads bypass this computer's cache on Windows and macOS.
use super::*;

pub(super) fn hash(path: &Path, size: u64, cancel: &AtomicBool) -> Result<u64, String> {
    let mut options = File::options();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // NO_BUFFERING: every request and its address are sector aligned. A full
        // aligned request at EOF may return a short final sector; hash only its data.
        // The staging handle remains writable until publication. Permit that
        // existing handle; do not permit its name to be removed during this read.
        options.custom_flags(0x2000_0000).share_mode(3);
    }
    let mut input = options.open(path).map_err(|e| e.to_string())?;
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::fcntl(input.as_raw_fd(), libc::F_NOCACHE, 1) } == -1 {
            return Err(io::Error::last_os_error().to_string());
        }
    }
    #[cfg(windows)]
    let mut buffer = Aligned::new();
    #[cfg(windows)]
    let buf = buffer.bytes();
    #[cfg(not(windows))]
    let mut buffer = vec![0u8; CHUNK];
    #[cfg(not(windows))]
    let buf = &mut buffer[..];
    let mut hash = Xxh64::new(0);
    let mut read = 0;
    loop {
        copies::cancelled(cancel)?;
        let n = input
            .read(buf)
            .map_err(|e| format!("Read-back failed: {e}"))?;
        if n == 0 {
            break;
        }
        read += n as u64;
        if read > size {
            return Err("Read-back file grew.".into());
        }
        hash.update(&buf[..n]);
        // An unaligned short read is EOF. A further unbuffered read would have
        // an unaligned offset (ERROR_INVALID_PARAMETER on Windows).
        #[cfg(windows)]
        if n < buf.len() {
            break;
        }
    }
    if read != size {
        return Err("Read-back file was truncated.".into());
    }
    Ok(hash.digest())
}

#[cfg(windows)]
struct Aligned(std::ptr::NonNull<u8>);
#[cfg(windows)]
impl Aligned {
    fn layout() -> std::alloc::Layout {
        // 64 KiB also covers shares reporting sectors larger than 4 KiB.
        std::alloc::Layout::from_size_align(CHUNK, 65536).unwrap()
    }
    fn new() -> Self {
        let ptr = unsafe { std::alloc::alloc_zeroed(Self::layout()) };
        Self(
            std::ptr::NonNull::new(ptr)
                .unwrap_or_else(|| std::alloc::handle_alloc_error(Self::layout())),
        )
    }
    fn bytes(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.0.as_ptr(), CHUNK) }
    }
}
#[cfg(windows)]
impl Drop for Aligned {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.0.as_ptr(), Self::layout()) };
    }
}

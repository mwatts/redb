#[cfg(any(unix, target_os = "wasi"))]
mod unix;
#[cfg(any(unix, target_os = "wasi"))]
pub use unix::FileBackend;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::FileBackend;

#[cfg(not(any(windows, unix, target_os = "wasi")))]
mod unsupported;
#[cfg(not(any(windows, unix, target_os = "wasi")))]
pub use unsupported::FileBackend;

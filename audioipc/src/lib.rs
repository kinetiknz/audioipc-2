// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details

#![warn(unused_extern_crates)]
#![recursion_limit = "1024"]
#[macro_use]
extern crate error_chain;
#[macro_use]
extern crate log;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate futures;
#[macro_use]
extern crate tokio_io;

mod async_msg;
#[cfg(unix)]
mod cmsg;
pub mod codec;
pub mod core;
#[allow(deprecated)]
pub mod errors;
#[cfg(unix)]
pub mod fd_passing;
#[cfg(unix)]
pub use crate::fd_passing as platformhandle_passing;
#[cfg(windows)]
pub mod handle_passing;
#[cfg(windows)]
pub use handle_passing as platformhandle_passing;
pub mod messages;
#[cfg(unix)]
mod msg;
pub mod rpc;
pub mod shm;

// TODO: Remove local fork when https://github.com/tokio-rs/tokio/pull/1294 is resolved.
#[cfg(unix)]
mod tokio_uds_stream;

#[cfg(windows)]
mod tokio_named_pipes;

pub use crate::messages::{ClientMessage, ServerMessage};

// TODO: Remove hardcoded size and allow allocation based on cubeb backend requirements.
pub const SHM_AREA_SIZE: usize = 2 * 1024 * 1024;

#[cfg(unix)]
use std::os::unix::io::IntoRawFd;
#[cfg(windows)]
use std::os::windows::io::IntoRawHandle;

// This must match the definition of
// ipc::FileDescriptor::PlatformHandleType in Gecko.
#[cfg(windows)]
pub type PlatformHandleType = std::os::windows::raw::HANDLE;
#[cfg(unix)]
pub type PlatformHandleType = libc::c_int;

// This stands in for RawFd/RawHandle.
#[derive(Debug)]
pub struct PlatformHandle(PlatformHandleType);

#[cfg(unix)]
pub const INVALID_HANDLE_VALUE: PlatformHandleType = -1isize as PlatformHandleType;

#[cfg(windows)]
pub const INVALID_HANDLE_VALUE: PlatformHandleType = winapi::um::handleapi::INVALID_HANDLE_VALUE;

#[cfg(unix)]
fn valid_handle(handle: PlatformHandleType) -> bool {
    handle >= 0
}

#[cfg(windows)]
fn valid_handle(handle: PlatformHandleType) -> bool {
    handle != INVALID_HANDLE_VALUE && handle != std::ptr::null_mut()
}

impl PlatformHandle {
    pub fn new(raw: PlatformHandleType) -> PlatformHandle {
        assert!(valid_handle(raw));
        PlatformHandle(raw)
    }

    #[cfg(windows)]
    pub fn from<T: IntoRawHandle>(from: T) -> PlatformHandle {
        PlatformHandle::new(from.into_raw_handle())
    }

    #[cfg(unix)]
    pub fn from<T: IntoRawFd>(from: T) -> PlatformHandle {
        PlatformHandle::new(from.into_raw_fd())
    }

    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn into_raw(self) -> PlatformHandleType {
        let handle = self.0;
        std::mem::forget(self);
        handle
    }

    #[cfg(unix)]
    pub fn duplicate(h: PlatformHandleType) -> Result<PlatformHandle, std::io::Error> {
        unsafe {
            let newfd = libc::dup(h);
            if newfd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(PlatformHandle::from(newfd))
        }
    }

    #[cfg(windows)]
    pub fn duplicate(h: PlatformHandleType) -> Result<PlatformHandle, std::io::Error> {
        let dup = unsafe { platformhandle_passing::duplicate_platformhandle(h, None, false) }?;
        Ok(PlatformHandle::new(dup))
    }
}

impl Drop for PlatformHandle {
    fn drop(&mut self) {
        unsafe { close_platformhandle(self.0) }
    }
}

#[cfg(unix)]
unsafe fn close_platformhandle(handle: PlatformHandleType) {
    libc::close(handle);
}

#[cfg(windows)]
unsafe fn close_platformhandle(handle: PlatformHandleType) {
    winapi::um::handleapi::CloseHandle(handle);
}

#[cfg(unix)]
pub mod messagestream_unix;
#[cfg(unix)]
pub use crate::messagestream_unix::*;

#[cfg(windows)]
pub mod messagestream_win;
#[cfg(windows)]
pub use messagestream_win::*;

#[cfg(windows)]
pub fn server_platform_init() {
    use winapi::shared::winerror;
    use winapi::um::combaseapi;
    use winapi::um::objbase;

    unsafe {
        let r = combaseapi::CoInitializeEx(std::ptr::null_mut(), objbase::COINIT_MULTITHREADED);
        assert!(winerror::SUCCEEDED(r));
    }
}

#[cfg(unix)]
pub fn server_platform_init() {}

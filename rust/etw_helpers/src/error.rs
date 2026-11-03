use core::fmt;

use windows_sys::Win32::Foundation::WIN32_ERROR;
use windows_sys::core::HRESULT;

/// An error from ETW or another Win32 API, identified by its `HRESULT`.
///
/// Win32 error codes are converted with `HRESULT_FROM_WIN32`, so for example
/// `Error::from_win32(ERROR_CANCELLED)` has code `0x800704C7`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Error(HRESULT);

impl Error {
    /// Creates an error from an `HRESULT`.
    pub const fn from_hresult(code: HRESULT) -> Self {
        Error(code)
    }

    /// Creates an error from a Win32 error code, as returned by `GetLastError` and the ETW APIs.
    pub const fn from_win32(code: WIN32_ERROR) -> Self {
        Error(hresult_from_win32(code))
    }

    /// Returns the error's `HRESULT`.
    pub const fn code(&self) -> HRESULT {
        self.0
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Error({:#010X})", self.0 as u32)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HRESULT {:#010X}", self.0 as u32)
    }
}

impl core::error::Error for Error {}

/// `HRESULT_FROM_WIN32`: maps a Win32 error code into the `FACILITY_WIN32` HRESULT space.
pub const fn hresult_from_win32(code: WIN32_ERROR) -> HRESULT {
    if code as HRESULT <= 0 {
        code as HRESULT
    } else {
        ((code & 0x0000_FFFF) | 0x8007_0000) as HRESULT
    }
}

// Some constants are only used by the optional event consumer backends.
#[allow(dead_code)]
mod codes {
    use super::*;
    use windows_sys::Win32::Foundation::{
        ERROR_CANCELLED, ERROR_INVALID_DATA, ERROR_NO_MORE_ITEMS, ERROR_WMI_INSTANCE_NOT_FOUND,
    };

    pub(crate) const S_OK: HRESULT = 0;
    pub(crate) const E_UNEXPECTED: HRESULT = 0x8000_FFFF_u32 as HRESULT;
    pub(crate) const E_CANCELLED: HRESULT = hresult_from_win32(ERROR_CANCELLED);
    pub(crate) const E_INVALID_DATA: HRESULT = hresult_from_win32(ERROR_INVALID_DATA);
    pub(crate) const ETW_SESSION_NOT_FOUND: HRESULT =
        hresult_from_win32(ERROR_WMI_INSTANCE_NOT_FOUND);
    // Consumers report that ProcessTrace has completed with this; `EventTrace` replaces it with
    // ProcessTrace's result, so it never reaches callers.
    pub(crate) const E_TRACE_FINISHED: HRESULT = hresult_from_win32(ERROR_NO_MORE_ITEMS);
}

pub(crate) use codes::*;

// Converts the HRESULTs passed around internally into the public error type.
pub(crate) trait IntoError {
    fn into_error(self) -> Error;
}

impl IntoError for HRESULT {
    fn into_error(self) -> Error {
        Error::from_hresult(self)
    }
}

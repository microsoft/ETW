#[allow(overflowing_literals)]
pub(crate) static E_CANCELLED: windows::core::HRESULT = windows::core::HRESULT(0x800704c7);
#[allow(overflowing_literals)]
pub(crate) static E_UNEXPECTED: windows::core::HRESULT = windows::core::HRESULT(0x8000ffff);
pub(crate) static S_OK: windows::core::HRESULT = windows::core::HRESULT(0);
// HRESULT_FROM_WIN32(ERROR_WMI_INSTANCE_NOT_FOUND)
pub(crate) static ETW_SESSION_NOT_FOUND: windows::core::HRESULT =
    windows::core::HRESULT(-2147020695);

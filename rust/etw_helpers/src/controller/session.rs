use alloc::borrow::Cow;
use alloc::string::ToString;
use core::mem::ManuallyDrop;

use windows_sys::Win32::{Foundation::ERROR_SUCCESS, System::Diagnostics::Etw::*};
use windows_sys::core::PCWSTR;

/// Gets a handle to an already-running ETW session by name.
///
/// The returned handle **stops the session when dropped**; call
/// [`ControlTraceHandle::manual_stop`] to leave it running.
///
/// # Panics
///
/// Panics if `session_name` is 1024 or more UTF-16 code units long.
pub fn get_existing_session(session_name: &str) -> Result<ControlTraceHandle, crate::Error> {
    let mut wsz_session_name: [u16; 1024] = [0; 1024];
    if !session_name.is_empty() {
        let name_utf16 = session_name.encode_utf16();

        let mut i = 0;
        for c in name_utf16 {
            wsz_session_name[i] = c;
            i += 1;

            if i >= 1024 {
                panic!();
            }
        }

        wsz_session_name[i] = 0; // nul-terminate
    }

    wsz_get_existing_session(wsz_session_name.as_ptr())
}

fn wsz_get_existing_session(wsz_session_name: PCWSTR) -> Result<ControlTraceHandle, crate::Error> {
    unsafe {
        let mut properties = EventTraceProperties::empty_for_query();
        let err = ControlTraceW(
            CONTROLTRACE_HANDLE { Value: 0 },
            wsz_session_name,
            &mut properties.props,
            EVENT_TRACE_CONTROL_QUERY,
        );
        if err != ERROR_SUCCESS {
            Err(crate::Error::from_win32(err))
        } else {
            Ok(ControlTraceHandle(CONTROLTRACE_HANDLE {
                Value: properties.props.Wnode.Anonymous1.HistoricalContext,
            }))
        }
    }
}

#[repr(C)]
pub struct EventTraceProperties {
    props: EVENT_TRACE_PROPERTIES,
    session_name: [u16; 1024],
    file_name: [u16; 1024],
}

impl EventTraceProperties {
    // Byte offsets of the inline name buffers, as ETW expects in LoggerNameOffset/LogFileNameOffset
    const SESSION_NAME_OFFSET: u32 =
        core::mem::offset_of!(EventTraceProperties, session_name) as u32;
    const FILE_NAME_OFFSET: u32 = core::mem::offset_of!(EventTraceProperties, file_name) as u32;

    fn empty_for_query() -> EventTraceProperties {
        unsafe {
            let mut props: EventTraceProperties = core::mem::zeroed();
            props.props.Wnode.BufferSize = core::mem::size_of::<Self>() as u32;
            props.props.Wnode.Flags = WNODE_FLAG_TRACED_GUID;

            props.props.LoggerNameOffset = Self::SESSION_NAME_OFFSET;
            props.props.LogFileNameOffset = Self::FILE_NAME_OFFSET;

            props
        }
    }

    /// Copies `session_name` into the inline logger name buffer, nul-terminated.
    /// An empty name leaves the properties unchanged.
    ///
    /// # Panics
    ///
    /// Panics if `session_name` is 1024 or more UTF-16 code units long.
    pub fn set_session_name(&mut self, session_name: &str) -> &mut Self {
        if !session_name.is_empty() {
            let name_utf16 = session_name.encode_utf16();

            let mut i = 0;
            for c in name_utf16 {
                self.session_name[i] = c;
                i += 1;

                if i >= 1024 {
                    panic!();
                }
            }

            self.session_name[i] = 0; // nul-terminate

            self.props.LoggerNameOffset = Self::SESSION_NAME_OFFSET;
        }

        self
    }

    /// Copies `file_name` into the inline log file name buffer, nul-terminated.
    /// An empty name leaves the properties unchanged.
    ///
    /// # Panics
    ///
    /// Panics if `file_name` is 1024 or more UTF-16 code units long.
    pub fn set_file_name(&mut self, file_name: &str) -> &mut Self {
        if !file_name.is_empty() {
            let name_utf16 = file_name.encode_utf16();

            let mut i = 0;
            for c in name_utf16 {
                self.file_name[i] = c;
                i += 1;

                if i >= 1024 {
                    panic!();
                }
            }

            self.file_name[i] = 0; // nul-terminate

            self.props.LogFileNameOffset = Self::FILE_NAME_OFFSET;
        }

        self
    }
}

pub struct ControlTraceHandle(CONTROLTRACE_HANDLE);

impl Drop for ControlTraceHandle {
    fn drop(&mut self) {
        let mut props = EventTraceProperties::empty_for_query();
        unsafe {
            let ptr = &mut props.props as *mut EVENT_TRACE_PROPERTIES;
            let _ = StopTraceW(self.0, core::ptr::null(), ptr);
        }
    }
}

// These values are all mutually exclusive
pub enum FileMode {
    Sequential,
    /// Overwrites the oldest events once the file reaches `maximum_file_size_in_kilobytes`.
    Circular {
        maximum_file_size_in_kilobytes: u32,
    },
    //Append, // Nobody should or does use this, so we're not going to implement it
    /// Starts a new file each time the current one reaches `maximum_file_size_in_kilobytes`.
    /// The file name must contain a `%d` format specifier.
    NewFile {
        maximum_file_size_in_kilobytes: u32,
    },
}

pub enum LogFileMode {
    File(FileMode),
    Buffering,
}

pub struct EtwSessionBuilder<FM> {
    //props: EventTraceProperties,
    session_name: Cow<'static, str>,
    file_name: Cow<'static, str>,
    buffer_size: u16,
    min_buffers: u16,
    max_buffers: u16,
    _maximum_file_size_in_kilobytes: u32,
    log_file_mode: LogFileMode,
    nonpaged: bool,
    _system_logger: bool,
    realtime: bool,
    _p: core::marker::PhantomData<FM>,
}

#[doc(hidden)]
pub trait SessionMode {}
#[doc(hidden)]
pub struct FileModeSession {}
impl SessionMode for FileModeSession {}
#[doc(hidden)]
pub struct BufferingModeSession {}
impl SessionMode for BufferingModeSession {}

// Options only available to file mode sessions
impl EtwSessionBuilder<FileModeSession> {
    /// Also delivers events to real-time consumers (see
    /// [`ProcessTraceHandle::from_session`](crate::consumer::ProcessTraceHandle::from_session))
    /// in addition to writing them to the log file.
    pub fn realtime_event_delivery(mut self) -> Self {
        self.realtime = true;
        self
    }
}

// Options only available to buffering mode sessions
impl EtwSessionBuilder<BufferingModeSession> {}

pub struct SessionBuilder {}

impl SessionBuilder {
    /// Creates a builder for a buffering-mode session, which keeps events in
    /// in-memory buffers only and does not write a log file.
    pub fn new_buffering_mode(session_name: &str) -> EtwSessionBuilder<BufferingModeSession> {
        let mut builder = EtwSessionBuilder::create(session_name);
        builder.log_file_mode = LogFileMode::Buffering;

        builder
    }

    /// Creates a builder for a session that logs events to `file_name` using `file_mode`.
    pub fn new_file_mode(
        session_name: &str,
        file_name: &str,
        file_mode: FileMode,
    ) -> EtwSessionBuilder<FileModeSession> {
        let mut builder = EtwSessionBuilder::create(session_name);
        builder.file_name = Cow::Owned(file_name.to_string());
        builder.log_file_mode = LogFileMode::File(file_mode);

        builder
    }
}

impl<FM> EtwSessionBuilder<FM>
where
    FM: SessionMode,
{
    #[doc(hidden)]
    fn create(session_name: &str) -> Self {
        EtwSessionBuilder::<FM> {
            session_name: Cow::Owned(session_name.to_string()),
            file_name: Cow::Borrowed(""),
            buffer_size: 64,
            min_buffers: 2,
            max_buffers: 2,
            _maximum_file_size_in_kilobytes: 0,
            log_file_mode: LogFileMode::Buffering,
            nonpaged: false,
            _system_logger: false,
            realtime: false,
            _p: core::marker::PhantomData,
        }
    }

    /// Sets the size of each session buffer in kilobytes. Defaults to 64.
    pub fn buffer_size_kb(mut self, buffer_size_in_kilobytes: u16) -> Self {
        self.buffer_size = buffer_size_in_kilobytes;
        self
    }

    /// Sets the minimum and maximum number of session buffers. Each defaults to 2.
    ///
    /// ETW allocates the minimum number of buffers when the session starts, and adds more, up to
    /// the maximum, as needed.
    pub fn buffer_counts(mut self, minimum_buffer_count: u16, maximum_buffer_count: u16) -> Self {
        self.min_buffers = minimum_buffer_count;
        self.max_buffers = maximum_buffer_count;
        self
    }

    /// Allocates session buffers from the nonpaged pool instead of the default paged pool.
    ///
    /// Required for sessions that receive events from kernel-mode providers, which
    /// cannot log to paged-memory sessions.
    pub fn nonpaged_pool(mut self) -> Self {
        self.nonpaged = true;
        self
    }

    /// Starts the session.
    ///
    /// If `recreate_existing_session` is true, any running session with the same name is
    /// stopped first; otherwise starting fails if the name is already in use.
    ///
    /// The returned handle **stops the session when dropped**; call
    /// [`ControlTraceHandle::manual_stop`] to leave it running.
    #[cfg_attr(feature = "hotpath", hotpath::measure(impl_type = "EtwSessionBuilder"))]
    pub fn start(
        self,
        recreate_existing_session: bool,
    ) -> Result<ControlTraceHandle, crate::Error> {
        // TODO: Validate

        let mut max_file_size = 0;

        let mut session_handle = CONTROLTRACE_HANDLE { Value: 0 };
        let mut properties = EventTraceProperties::empty_for_query();
        properties.set_session_name(&self.session_name);

        let mut log_file_mode;

        if let LogFileMode::File(file_mode) = self.log_file_mode {
            match file_mode {
                FileMode::Circular {
                    maximum_file_size_in_kilobytes: kb,
                } => {
                    max_file_size = kb;
                    log_file_mode = EVENT_TRACE_FILE_MODE_CIRCULAR;
                }
                FileMode::Sequential => {
                    log_file_mode = EVENT_TRACE_FILE_MODE_SEQUENTIAL;
                }
                FileMode::NewFile {
                    maximum_file_size_in_kilobytes: kb,
                } => {
                    max_file_size = kb;
                    log_file_mode = EVENT_TRACE_FILE_MODE_NEWFILE;
                }
            }

            // MaximumFileSize is in megabytes unless this flag is set.
            if max_file_size != 0 {
                log_file_mode |= EVENT_TRACE_USE_KBYTES_FOR_SIZE;
            }

            properties.set_file_name(&self.file_name);

            if self.realtime {
                log_file_mode |= EVENT_TRACE_REAL_TIME_MODE;
            }
        } else {
            log_file_mode = EVENT_TRACE_BUFFERING_MODE;
            properties.props.LogFileNameOffset = 0;
        }

        if !self.nonpaged {
            log_file_mode |= EVENT_TRACE_USE_PAGED_MEMORY;
        }

        properties.props = EVENT_TRACE_PROPERTIES {
            Wnode: WNODE_HEADER {
                ClientContext: 1,

                ..properties.props.Wnode
            },
            BufferSize: self.buffer_size as u32,
            MinimumBuffers: self.min_buffers as u32,
            MaximumBuffers: self.max_buffers as u32,
            LogFileMode: log_file_mode,
            FlushTimer: 1,
            MaximumFileSize: max_file_size,

            ..properties.props
        };

        unsafe {
            if recreate_existing_session {
                let existing_session = wsz_get_existing_session(properties.session_name.as_ptr());
                if existing_session.is_ok() {
                    // Stop existing session
                    drop(existing_session);
                } else if existing_session.is_err()
                    && existing_session.as_ref().unwrap_err_unchecked().code()
                        == crate::error::ETW_SESSION_NOT_FOUND
                {
                    // No existing session
                } else {
                    // There's be an error, but ignore it and use the error StartTrace returns instead
                }
            }

            let ptr = &mut properties.props as *mut EVENT_TRACE_PROPERTIES;
            let err = StartTraceW(&mut session_handle, properties.session_name.as_ptr(), ptr);

            if err != ERROR_SUCCESS {
                Err(crate::Error::from_win32(err))
            } else {
                Ok(ControlTraceHandle(session_handle))
            }
        }
    }
}

impl ControlTraceHandle {
    /// Prevents the session from being stopped when the handle is dropped, so it
    /// keeps running after this process is done with it.
    pub fn manual_stop(self) -> ManuallyDrop<Self> {
        ManuallyDrop::new(self)
    }

    /// Enables the provider with GUID `provider_id` in this session, for events at or below
    /// `level` (e.g. `0xFF` for all levels). No keyword filtering is applied.
    pub fn enable_provider(
        &self,
        provider_id: &windows_sys::core::GUID,
        level: u8,
    ) -> Result<(), crate::Error> {
        unsafe {
            let err = EnableTraceEx2(
                self.0,
                provider_id,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER,
                level,
                0,
                0,
                0,
                core::ptr::null(),
            );
            if err != ERROR_SUCCESS {
                Err(crate::Error::from_win32(err))
            } else {
                Ok(())
            }
        }
    }

    /// Disables the provider with GUID `provider_id` in this session.
    pub fn disable_provider(
        &self,
        provider_id: &windows_sys::core::GUID,
    ) -> Result<(), crate::Error> {
        unsafe {
            let err = EnableTraceEx2(
                self.0,
                provider_id,
                EVENT_CONTROL_CODE_DISABLE_PROVIDER,
                0xFF,
                0,
                0,
                0,
                core::ptr::null(),
            );
            if err != ERROR_SUCCESS {
                Err(crate::Error::from_win32(err))
            } else {
                Ok(())
            }
        }
    }
}

#[cfg(test)]
#[allow(non_upper_case_globals)]
mod tests {
    use super::*;

    fn query_log_file_mode(session_name: &str) -> Result<u32, crate::Error> {
        let mut properties = EventTraceProperties::empty_for_query();
        properties.set_session_name(session_name);
        let err = unsafe {
            ControlTraceW(
                CONTROLTRACE_HANDLE { Value: 0 },
                properties.session_name.as_ptr(),
                &mut properties.props,
                EVENT_TRACE_CONTROL_QUERY,
            )
        };
        if err != ERROR_SUCCESS {
            return Err(crate::Error::from_win32(err));
        }

        Ok(properties.props.LogFileMode)
    }

    #[test]
    fn paged_pool_by_default() -> Result<(), crate::Error> {
        const test_name: &str = "EtwSession-Rust-Tests-Paged";

        let _h = SessionBuilder::new_buffering_mode(test_name).start(true)?;

        assert_ne!(
            query_log_file_mode(test_name)? & EVENT_TRACE_USE_PAGED_MEMORY,
            0
        );

        Ok(())
    }

    #[test]
    fn nonpaged_pool() -> Result<(), crate::Error> {
        const test_name: &str = "EtwSession-Rust-Tests-Nonpaged";

        let _h = SessionBuilder::new_buffering_mode(test_name)
            .nonpaged_pool()
            .start(true)?;

        assert_eq!(
            query_log_file_mode(test_name)? & EVENT_TRACE_USE_PAGED_MEMORY,
            0
        );

        Ok(())
    }

    #[test]
    fn circular_file() -> Result<(), crate::Error> {
        const test_name: &str = "EtwSession-Rust-Tests-Circular";
        let file_name = crate::test_util::etl_path("circular.etl");

        // ETW rejects a circular log file without a maximum size.
        let _h = SessionBuilder::new_file_mode(
            test_name,
            &file_name,
            FileMode::Circular {
                maximum_file_size_in_kilobytes: 1024,
            },
        )
        .start(true)?;

        assert_ne!(
            query_log_file_mode(test_name)? & EVENT_TRACE_FILE_MODE_CIRCULAR,
            0
        );

        Ok(())
    }
}

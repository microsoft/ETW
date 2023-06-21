use std::{borrow::Cow, mem::ManuallyDrop};

use windows::{core::PCSTR, Win32::System::Diagnostics::Etw::*};

#[repr(C)]
pub struct EventTraceProperties {
    props: EVENT_TRACE_PROPERTIES,
    session_name: [u8; 1024],
    file_name: [u8; 1024],
}

impl EventTraceProperties {
    fn empty_for_query() -> EventTraceProperties {
        unsafe {
            let mut props: EventTraceProperties = core::mem::zeroed();
            props.props.Wnode.BufferSize = core::mem::size_of::<Self>() as u32;
            props.props.Wnode.Flags = WNODE_FLAG_TRACED_GUID;

            props.props.LoggerNameOffset = core::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
            props.props.LogFileNameOffset =
                core::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32 + 1024;

            props
        }
    }

    pub fn set_session_name(&mut self, session_name: &str) -> &mut Self {
        if !session_name.is_empty() {
            unsafe {
                let len = session_name.len();
                if len >= 1024 {
                    panic!()
                }

                // TODO: Can we actually use UTF-8 as a session name?
                core::ptr::copy_nonoverlapping(
                    session_name.as_ptr(),
                    self.session_name.as_mut_ptr(),
                    len,
                );

                self.session_name[len] = b'\0';

                self.props.LoggerNameOffset = core::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
            }
        }

        self
    }

    pub fn set_file_name(&mut self, file_name: &str) -> &mut Self {
        if !file_name.is_empty() {
            unsafe {
                let len = file_name.len();
                if len >= 1024 {
                    panic!()
                }

                core::ptr::copy_nonoverlapping(
                    file_name.as_ptr(),
                    self.file_name.as_mut_ptr(),
                    len,
                );

                self.file_name[len] = b'\0';

                self.props.LogFileNameOffset =
                    core::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32 + 1024;
            }
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
            let _ = StopTraceA(self.0, PCSTR::null(), ptr);
        }
    }
}

pub struct EtwSession(core::marker::PhantomData<&'static bool>);

impl EtwSession {
    pub fn get_etw_session(
        sz_session_name: PCSTR,
    ) -> Result<ControlTraceHandle, windows::core::Error> {
        unsafe {
            let mut properties = EventTraceProperties::empty_for_query();
            let err = ControlTraceA(
                CONTROLTRACE_HANDLE::default(),
                sz_session_name,
                &mut properties.props,
                EVENT_TRACE_CONTROL_QUERY,
            );
            if err.is_err() {
                Err(err.into())
            } else {
                Ok(ControlTraceHandle(CONTROLTRACE_HANDLE(
                    properties.props.Wnode.Anonymous1.HistoricalContext,
                )))
            }
        }
    }
}

// These values are all mutually exclusive
pub enum FileMode {
    Sequential,
    Circular,
    //Append, // Nobody should or does use this, so we're not going to implement it
    NewFile { maximum_file_size_in_kilobytes: u32 },
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
    num_buffers: u16,
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
    pub fn realtime_event_delivery(mut self) -> Self {
        self.realtime = true;
        self
    }
}

// Options only available to buffering mode sessions
impl EtwSessionBuilder<BufferingModeSession> {}

pub struct SessionBuilder {}

impl SessionBuilder {
    pub fn new_buffering_mode(session_name: &str) -> EtwSessionBuilder<BufferingModeSession> {
        let mut builder = EtwSessionBuilder::create(session_name);
        builder.log_file_mode = LogFileMode::Buffering;

        builder
    }

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
            num_buffers: 2,
            _maximum_file_size_in_kilobytes: 0,
            log_file_mode: LogFileMode::Buffering,
            nonpaged: false,
            _system_logger: false,
            realtime: false,
            _p: core::marker::PhantomData,
        }
    }

    pub fn buffer_size_kb(mut self, buffer_size_in_kilobytes: u16) -> Self {
        self.buffer_size = buffer_size_in_kilobytes;
        self
    }

    pub fn buffer_counts(
        mut self,
        minimum_buffer_count: u16,
        starting_buffer_count: u16,
        maximum_buffer_count: u16,
    ) -> Self {
        self.min_buffers = minimum_buffer_count;
        self.num_buffers = starting_buffer_count;
        self.max_buffers = maximum_buffer_count;
        self
    }

    pub fn nonpaged_pool(mut self) -> Self {
        self.nonpaged = true;
        self
    }

    pub fn start(
        self,
        recreate_existing_session: bool,
    ) -> Result<ControlTraceHandle, windows::core::Error> {
        // TODO: Validate

        let mut max_file_size = 0;

        let mut session_handle: CONTROLTRACE_HANDLE = Default::default();
        let mut properties = EventTraceProperties::empty_for_query();
        properties.set_session_name(&self.session_name);

        let mut log_file_mode;

        if let LogFileMode::File(file_mode) = self.log_file_mode {
            match file_mode {
                FileMode::Circular => {
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

            properties.set_file_name(&self.file_name);

            if self.realtime {
                log_file_mode |= EVENT_TRACE_REAL_TIME_MODE;
            }
        } else {
            log_file_mode = EVENT_TRACE_BUFFERING_MODE;
            properties.props.LogFileNameOffset = 0;
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
            NumberOfBuffers: self.num_buffers as u32,
            FlushTimer: 1,
            MaximumFileSize: max_file_size,

            ..properties.props
        };

        unsafe {
            if recreate_existing_session {
                let existing_session =
                    EtwSession::get_etw_session(PCSTR(properties.session_name.as_ptr()));
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
            let err = StartTraceA(
                &mut session_handle,
                PCSTR(properties.session_name.as_ptr()),
                ptr,
            );

            if err.is_err() {
                Err(err.into())
            } else {
                Ok(ControlTraceHandle(session_handle))
            }
        }
    }
}

impl ControlTraceHandle {
    pub fn manual_stop(self) -> ManuallyDrop<Self> {
        ManuallyDrop::new(self)
    }

    pub fn enable_provider(
        &self,
        provider_id: &windows::core::GUID,
    ) -> Result<(), windows::core::Error> {
        unsafe {
            let err = EnableTraceEx2(
                self.0,
                provider_id,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
                0xFF,
                0,
                0,
                0,
                None,
            );
            if err.is_err() {
                Err(err.into())
            } else {
                Ok(())
            }
        }
    }

    pub fn disable_provider(
        &self,
        provider_id: &windows::core::GUID,
    ) -> Result<(), windows::core::Error> {
        unsafe {
            let err = EnableTraceEx2(
                self.0,
                provider_id,
                EVENT_CONTROL_CODE_DISABLE_PROVIDER.0,
                0xFF,
                0,
                0,
                0,
                None,
            );
            if err.is_err() {
                Err(err.into())
            } else {
                Ok(())
            }
        }
    }
}

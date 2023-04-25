use std::{ffi::c_void, mem::ManuallyDrop};

use windows::{
    core::{PCSTR, PSTR},
    Win32::System::Diagnostics::Etw::*,
};

// HRESULT_FROM_WIN32(ERROR_WMI_INSTANCE_NOT_FOUND)
const ETW_SESSION_NOT_FOUND: windows::core::HRESULT = windows::core::HRESULT(-2147020695);

#[repr(C)]
pub struct EventTraceProperties {
    props: EVENT_TRACE_PROPERTIES,
    file_name: [u8; 1024],
    session_name: [u8; 1024],
}

impl EventTraceProperties {
    pub fn new(for_query: bool) -> EventTraceProperties {
        unsafe {
            let mut props: EventTraceProperties = core::mem::zeroed();
            props.props.Wnode.BufferSize = core::mem::size_of::<Self>() as u32;
            props.props.Wnode.Flags = WNODE_FLAG_TRACED_GUID;

            if for_query {
                props.props.LoggerNameOffset =
                    core::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
                props.props.LogFileNameOffset =
                    core::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32 + 1024;
            }

            props
        }
    }

    pub fn set_session_name(&mut self, session_name: PCSTR) -> &mut Self {
        if !session_name.is_null() {
            unsafe {
                let len = windows::core::strlen(session_name) + 1;
                if len > 1024 {
                    panic!()
                }

                core::ptr::copy_nonoverlapping(
                    session_name.as_ptr(),
                    self.session_name.as_mut_ptr(),
                    len,
                );
                self.props.LoggerNameOffset = core::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
            }
        }

        self
    }

    #[allow(dead_code)]
    pub fn set_file_name(&mut self, file_name: PCSTR) -> &mut Self {
        if !file_name.is_null() {
            unsafe {
                let len = windows::core::strlen(file_name) + 1;
                if len > 1024 {
                    panic!()
                }

                core::ptr::copy_nonoverlapping(
                    file_name.as_ptr(),
                    self.file_name.as_mut_ptr(),
                    len,
                );
                self.props.LogFileNameOffset =
                    core::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32 + 1024;
            }
        }

        self
    }
}

#[repr(C)]
pub struct EventTraceLogFile {
    pub(crate) props: EVENT_TRACE_LOGFILEA,
    name: [u8; 1024],
}

impl EventTraceLogFile {
    #[allow(dead_code)]
    pub fn from_session(
        session_name: PCSTR,
        callback: PEVENT_RECORD_CALLBACK,
    ) -> EventTraceLogFile {
        unsafe {
            if session_name.is_null() {
                panic!()
            }

            let mut props: EventTraceLogFile = core::mem::zeroed();
            props.props.Anonymous1.ProcessTraceMode =
                PROCESS_TRACE_MODE_EVENT_RECORD | PROCESS_TRACE_MODE_REAL_TIME;
            props.props.Anonymous2.EventRecordCallback = callback;

            let len = windows::core::strlen(session_name) + 1;
            if len > 1024 {
                panic!()
            }

            core::ptr::copy_nonoverlapping(session_name.as_ptr(), props.name.as_mut_ptr(), len);
            props.props.LoggerName = PSTR::from_raw(props.name.as_mut_ptr());

            props
        }
    }

    #[allow(dead_code)]
    pub fn from_file(file_name: PCSTR, callback: PEVENT_RECORD_CALLBACK) -> EventTraceLogFile {
        unsafe {
            if file_name.is_null() {
                panic!()
            }

            let mut props: EventTraceLogFile = core::mem::zeroed();
            props.props.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_EVENT_RECORD;
            props.props.Anonymous2.EventRecordCallback = callback;

            let len = windows::core::strlen(file_name) + 1;
            if len > 1024 {
                panic!()
            }

            core::ptr::copy_nonoverlapping(file_name.as_ptr(), props.name.as_mut_ptr(), len);
            props.props.LogFileName = PSTR::from_raw(props.name.as_mut_ptr());

            props
        }
    }

    #[allow(dead_code)]
    pub(crate) fn set_user_context(mut self, ctx: *const c_void) -> Self {
        self.props.Context = ctx as *mut c_void;

        self
    }
}

pub struct ControlTraceHandle(CONTROLTRACE_HANDLE);

impl Drop for ControlTraceHandle {
    fn drop(&mut self) {
        let mut props = EventTraceProperties::new(true);
        unsafe {
            let ptr = &mut props.props as *mut EVENT_TRACE_PROPERTIES;
            let _ = StopTraceA(self.0, PCSTR::null(), ptr);
        }
    }
}

pub struct EtwSession(core::marker::PhantomData<&'static bool>);

impl EtwSession {
    pub fn start_etw_session(
        sz_session_name: PCSTR,
    ) -> Result<ControlTraceHandle, windows::core::Error> {
        let mut session_handle: CONTROLTRACE_HANDLE = Default::default();
        let mut properties = EventTraceProperties::new(false);
        properties.set_session_name(sz_session_name);
        properties.props = EVENT_TRACE_PROPERTIES {
            Wnode: WNODE_HEADER {
                ClientContext: 1,

                ..properties.props.Wnode
            },
            BufferSize: 64,
            MinimumBuffers: 16,
            MaximumBuffers: 16,
            LogFileMode: EVENT_TRACE_FILE_MODE_NONE | EVENT_TRACE_REAL_TIME_MODE,
            NumberOfBuffers: 16,
            FlushTimer: 1,

            ..properties.props
        };

        unsafe {
            let ptr = &mut properties.props as *mut EVENT_TRACE_PROPERTIES;
            let err = StartTraceA(&mut session_handle, sz_session_name, ptr);

            if err.is_err() {
                Err(err.into())
            } else {
                Ok(ControlTraceHandle(session_handle))
            }
        }
    }

    #[allow(dead_code)]
    pub fn get_etw_session(
        sz_session_name: PCSTR,
    ) -> Result<ControlTraceHandle, windows::core::Error> {
        unsafe {
            let mut properties = EventTraceProperties::new(true);
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

    #[allow(dead_code)]
    pub fn get_or_start_etw_session(
        sz_session_name: PCSTR,
        recreate_existing_session: bool,
    ) -> Result<ControlTraceHandle, windows::core::Error> {
        unsafe {
            let existing_session = EtwSession::get_etw_session(sz_session_name);
            if existing_session.is_ok() && recreate_existing_session {
                drop(existing_session);
                EtwSession::start_etw_session(sz_session_name)
            } else if existing_session.is_err()
                && existing_session.as_ref().unwrap_err_unchecked().code()
                    == ETW_SESSION_NOT_FOUND.into()
            {
                EtwSession::start_etw_session(sz_session_name)
            } else {
                existing_session
            }
        }
    }
}

impl ControlTraceHandle {
    #[allow(dead_code)]
    pub fn manual_stop(self) -> ManuallyDrop<Self> {
        ManuallyDrop::new(self)
    }

    #[allow(dead_code)]
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

    #[allow(dead_code)]
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

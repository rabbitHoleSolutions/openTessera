//! libmpv render API wrapper.
//!
//! Loads `libmpv.so.1` (or `$TESSERA_MPV_LIB`) at runtime via dlopen and
//! declares the C ABI by hand, so no system development headers are needed.
//! Requires libmpv client API >= 1.0 with the OpenGL render backend.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_double, c_int, c_void};
use std::sync::Arc;

use libloading::Library;

pub type MpvHandlePtr = *mut c_void;
pub type RenderContextPtr = *mut c_void;

const MPV_FORMAT_STRING: c_int = 1;
const MPV_FORMAT_INT64: c_int = 4;
const MPV_FORMAT_DOUBLE: c_int = 5;

const MPV_RENDER_PARAM_API_TYPE: c_int = 1;
const MPV_RENDER_PARAM_OPENGL_INIT_PARAMS: c_int = 2;
const MPV_RENDER_PARAM_OPENGL_FBO: c_int = 3;
const MPV_RENDER_PARAM_FLIP_Y: c_int = 4;
const MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME: c_int = 12;

pub const MPV_RENDER_UPDATE_FRAME: u64 = 1;

const MPV_EVENT_NONE: c_int = 0;
const MPV_EVENT_SHUTDOWN: c_int = 1;
const MPV_EVENT_START_FILE: c_int = 6;
const MPV_EVENT_END_FILE: c_int = 7;
const MPV_EVENT_FILE_LOADED: c_int = 8;
const MPV_EVENT_IDLE: c_int = 11;

#[repr(C)]
struct RenderParam {
    kind: c_int,
    data: *mut c_void,
}

#[repr(C)]
pub struct MpvGlInitParams {
    pub get_proc_address: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void>,
    pub get_proc_address_ctx: *mut c_void,
    pub extra_exts: *const c_char,
}

#[repr(C)]
struct MpvGlFbo {
    fbo: c_int,
    w: c_int,
    h: c_int,
    internal_format: c_int,
}

#[repr(C)]
struct RawMpvEvent {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

#[repr(C)]
struct MpvEventEndFile {
    reason: c_int,
    error: c_int,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndFileReason {
    Eof,
    Stop,
    Quit,
    Error,
    Redirect,
    Unknown,
}

#[derive(Debug, Clone)]
pub enum MpvEvent {
    Shutdown,
    StartFile,
    EndFile { reason: EndFileReason, error: i32 },
    FileLoaded,
    Idle,
    Other(i32),
}

#[derive(Debug)]
pub enum Error {
    LibraryOpen(String),
    Symbol(String),
    UnsupportedVersion(u64),
    Mpv(i32, String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::LibraryOpen(s) => write!(f, "failed to open libmpv: {s}"),
            Error::Symbol(s) => write!(f, "missing libmpv symbol: {s}"),
            Error::UnsupportedVersion(v) => write!(f, "unsupported libmpv client API version: 0x{v:X}"),
            Error::Mpv(code, s) => write!(f, "libmpv error {code} ({s})"),
        }
    }
}

impl std::error::Error for Error {}

fn error_name(code: c_int) -> &'static str {
    match code {
        -1 => "EVENT_QUEUE_FULL",
        -2 => "NOMEM",
        -3 => "UNINITIALIZED",
        -4 => "INVALID_PARAMETER",
        -5 => "OPTION_NOT_FOUND",
        -6 => "OPTION_FORMAT",
        -7 => "OPTION_ERROR",
        -8 => "PROPERTY_NOT_FOUND",
        -9 => "PROPERTY_FORMAT",
        -10 => "PROPERTY_UNAVAILABLE",
        -11 => "PROPERTY_ERROR",
        -12 => "COMMAND",
        -13 => "LOADING_FAILED",
        -14 => "AO_INIT_FAILED",
        -15 => "VO_INIT_FAILED",
        -16 => "NOTHING_TO_PLAY",
        -17 => "UNKNOWN_FORMAT",
        -18 => "UNSUPPORTED",
        -19 => "NOT_IMPLEMENTED",
        -20 => "GENERIC",
        _ => "UNKNOWN",
    }
}

pub struct MpvLib {
    _lib: Library,
    create: unsafe extern "C" fn() -> MpvHandlePtr,
    initialize: unsafe extern "C" fn(MpvHandlePtr) -> c_int,
    terminate_destroy: unsafe extern "C" fn(MpvHandlePtr),
    set_option_string: unsafe extern "C" fn(MpvHandlePtr, *const c_char, *const c_char) -> c_int,
    command: unsafe extern "C" fn(MpvHandlePtr, *const *const c_char) -> c_int,
    // NB: mpv's ABI here puts reply_userdata SECOND (client.h 0.34:
    // int mpv_command_async(mpv_handle*, uint64_t reply_userdata,
    //                       const char **args)) — passing args/userdata in
    // the documented-in-my-head order segfaults inside libmpv.
    command_async: unsafe extern "C" fn(MpvHandlePtr, u64, *const *const c_char) -> c_int,
    get_property: unsafe extern "C" fn(MpvHandlePtr, *const c_char, c_int, *mut c_void) -> c_int,
    free: unsafe extern "C" fn(*mut c_void),
    client_api_version: unsafe extern "C" fn() -> u64,
    wait_event: unsafe extern "C" fn(MpvHandlePtr, c_double) -> *mut RawMpvEvent,
    render_context_create: unsafe extern "C" fn(*mut RenderContextPtr, MpvHandlePtr, *const RenderParam) -> c_int,
    render_context_update: unsafe extern "C" fn(RenderContextPtr) -> u64,
    render_context_render: unsafe extern "C" fn(RenderContextPtr, *const RenderParam) -> c_int,
    render_context_free: unsafe extern "C" fn(RenderContextPtr),
}

macro_rules! sym {
    ($lib:expr, $name:ident) => {
        $lib.get(stringify!($name).as_bytes())
            .map(|s| *s)
            .map_err(|_| Error::Symbol(stringify!($name).to_string()))?
    };
}

impl MpvLib {
    pub fn load() -> Result<Arc<Self>, Error> {
        let path = std::env::var("TESSERA_MPV_LIB").unwrap_or_else(|_| "libmpv.so.1".to_string());
        unsafe {
            let lib = Library::new(&path).map_err(|e| Error::LibraryOpen(format!("{path}: {e}")))?;
            let this = MpvLib {
                create: sym!(lib, mpv_create),
                initialize: sym!(lib, mpv_initialize),
                terminate_destroy: sym!(lib, mpv_terminate_destroy),
                set_option_string: sym!(lib, mpv_set_option_string),
                command: sym!(lib, mpv_command),
                command_async: sym!(lib, mpv_command_async),
                get_property: sym!(lib, mpv_get_property),
                free: sym!(lib, mpv_free),
                client_api_version: sym!(lib, mpv_client_api_version),
                wait_event: sym!(lib, mpv_wait_event),
                render_context_create: sym!(lib, mpv_render_context_create),
                render_context_update: sym!(lib, mpv_render_context_update),
                render_context_render: sym!(lib, mpv_render_context_render),
                render_context_free: sym!(lib, mpv_render_context_free),
                _lib: lib,
            };
            let version = (this.client_api_version)();
            if version < (1u64 << 16) {
                return Err(Error::UnsupportedVersion(version));
            }
            Ok(Arc::new(this))
        }
    }

    pub fn client_api_version(&self) -> u64 {
        unsafe { (self.client_api_version)() }
    }
}

pub struct Mpv {
    lib: Arc<MpvLib>,
    handle: MpvHandlePtr,
}

unsafe impl Send for Mpv {}
unsafe impl Sync for Mpv {}

impl Mpv {
    pub fn new(lib: &Arc<MpvLib>, options: &[(&str, &str)]) -> Result<Self, Error> {
        unsafe {
            let handle = (lib.create)();
            if handle.is_null() {
                return Err(Error::Mpv(-20, "mpv_create returned null".into()));
            }
            let mpv = Mpv { lib: lib.clone(), handle };
            for (k, v) in options {
                mpv.set_option(k, v)?;
            }
            let r = (lib.initialize)(handle);
            if r < 0 {
                (lib.terminate_destroy)(handle);
                return Err(Error::Mpv(r, error_name(r).into()));
            }
            Ok(mpv)
        }
    }

    pub fn set_option(&self, key: &str, value: &str) -> Result<(), Error> {
        let k = CString::new(key).map_err(|_| Error::Mpv(-4, "option key contains NUL".into()))?;
        let v = CString::new(value).map_err(|_| Error::Mpv(-4, "option value contains NUL".into()))?;
        let r = unsafe { (self.lib.set_option_string)(self.handle, k.as_ptr(), v.as_ptr()) };
        if r < 0 {
            Err(Error::Mpv(r, error_name(r).into()))
        } else {
            Ok(())
        }
    }

    pub fn command(&self, args: &[&str]) -> Result<(), Error> {
        let cstrs: Vec<CString> = args
            .iter()
            .map(|s| CString::new(*s).map_err(|_| Error::Mpv(-4, "command contains NUL".into())))
            .collect::<Result<_, _>>()?;
        let mut ptrs: Vec<*const c_char> = cstrs.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        let r = unsafe { (self.lib.command)(self.handle, ptrs.as_ptr()) };
        if r < 0 {
            Err(Error::Mpv(r, error_name(r).into()))
        } else {
            Ok(())
        }
    }

    /// Queues a command for asynchronous execution. Returns once the
    /// command is queued (not completed) — the reply event is ignored.
    /// Use for anything that can block on I/O (seeks, loads), so the
    /// render loop never stalls.
    pub fn command_async(&self, args: &[&str]) -> Result<(), Error> {
        let cstrs: Vec<CString> = args
            .iter()
            .map(|s| CString::new(*s).map_err(|_| Error::Mpv(-4, "command contains NUL".into())))
            .collect::<Result<_, _>>()?;
        let mut ptrs: Vec<*const c_char> = cstrs.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        let r = unsafe { (self.lib.command_async)(self.handle, 0, ptrs.as_ptr()) };
        if r < 0 {
            Err(Error::Mpv(r, error_name(r).into()))
        } else {
            Ok(())
        }
    }

    pub fn load_file_async(&self, path: &str, mode: &str) -> Result<(), Error> {
        self.command_async(&["loadfile", path, mode])
    }

    pub fn load_file(&self, path: &str, mode: &str) -> Result<(), Error> {
        self.command(&["loadfile", path, mode])
    }

    pub fn get_i64(&self, prop: &str) -> Option<i64> {
        let name = CString::new(prop).ok()?;
        let mut val: i64 = 0;
        let r = unsafe {
            (self.lib.get_property)(self.handle, name.as_ptr(), MPV_FORMAT_INT64, &mut val as *mut i64 as *mut c_void)
        };
        if r < 0 { None } else { Some(val) }
    }

    pub fn get_f64(&self, prop: &str) -> Option<f64> {
        let name = CString::new(prop).ok()?;
        let mut val: f64 = 0.0;
        let r = unsafe {
            (self.lib.get_property)(self.handle, name.as_ptr(), MPV_FORMAT_DOUBLE, &mut val as *mut f64 as *mut c_void)
        };
        if r < 0 { None } else { Some(val) }
    }

    pub fn get_string(&self, prop: &str) -> Option<String> {
        let name = CString::new(prop).ok()?;
        let mut val: *mut c_char = std::ptr::null_mut();
        let r = unsafe {
            (self.lib.get_property)(self.handle, name.as_ptr(), MPV_FORMAT_STRING, &mut val as *mut *mut c_char as *mut c_void)
        };
        if r < 0 || val.is_null() {
            return None;
        }
        let s = unsafe { CStr::from_ptr(val) }.to_string_lossy().into_owned();
        unsafe { (self.lib.free)(val as *mut c_void) };
        Some(s)
    }

    pub fn poll_event(&self) -> Option<MpvEvent> {
        unsafe {
            let ev = (self.lib.wait_event)(self.handle, 0.0);
            if ev.is_null() || (*ev).event_id == MPV_EVENT_NONE {
                return None;
            }
            let id = (*ev).event_id;
            let event = match id {
                MPV_EVENT_SHUTDOWN => MpvEvent::Shutdown,
                MPV_EVENT_START_FILE => MpvEvent::StartFile,
                MPV_EVENT_END_FILE => {
                    let (reason, error) = if (*ev).data.is_null() {
                        (EndFileReason::Unknown, 0)
                    } else {
                        let d = &*((*ev).data as *mut MpvEventEndFile);
                        let reason = match d.reason {
                            0 => EndFileReason::Eof,
                            2 => EndFileReason::Stop,
                            3 => EndFileReason::Quit,
                            4 => EndFileReason::Error,
                            5 => EndFileReason::Redirect,
                            _ => EndFileReason::Unknown,
                        };
                        (reason, d.error)
                    };
                    MpvEvent::EndFile { reason, error }
                }
                MPV_EVENT_FILE_LOADED => MpvEvent::FileLoaded,
                MPV_EVENT_IDLE => MpvEvent::Idle,
                _ => MpvEvent::Other(id),
            };
            Some(event)
        }
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        unsafe { (self.lib.terminate_destroy)(self.handle) };
    }
}

pub struct RenderContext {
    lib: Arc<MpvLib>,
    ctx: RenderContextPtr,
}

impl RenderContext {
    /// Creates an OpenGL render context. The caller must have a current GL
    /// context on the calling thread, and `get_proc_address` must resolve GL
    /// symbols for it. The `Mpv` handle must outlive the `RenderContext`
    /// (drop the render context first).
    pub fn new_gl(
        mpv: &Mpv,
        get_proc_address: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
        get_proc_address_ctx: *mut c_void,
    ) -> Result<Self, Error> {
        let api_type = CString::new("opengl").unwrap();
        let mut init_params = MpvGlInitParams {
            get_proc_address: Some(get_proc_address),
            get_proc_address_ctx,
            extra_exts: std::ptr::null(),
        };
        let params: [RenderParam; 3] = [
            RenderParam { kind: MPV_RENDER_PARAM_API_TYPE, data: api_type.as_ptr() as *mut c_void },
            RenderParam { kind: MPV_RENDER_PARAM_OPENGL_INIT_PARAMS, data: &mut init_params as *mut MpvGlInitParams as *mut c_void },
            RenderParam { kind: 0, data: std::ptr::null_mut() },
        ];
        let mut ctx: RenderContextPtr = std::ptr::null_mut();
        let r = unsafe { (mpv.lib.render_context_create)(&mut ctx, mpv.handle, params.as_ptr()) };
        if r < 0 || ctx.is_null() {
            return Err(Error::Mpv(r, error_name(r).into()));
        }
        Ok(RenderContext { lib: mpv.lib.clone(), ctx })
    }

    pub fn update(&self) -> u64 {
        unsafe { (self.lib.render_context_update)(self.ctx) }
    }

    pub fn render(&self, fbo: i32, w: i32, h: i32, flip_y: bool) -> Result<(), Error> {
        let mut fbo_data = MpvGlFbo { fbo, w, h, internal_format: 0 };
        let mut flip: c_int = c_int::from(flip_y);
        let mut block: c_int = 0;
        let params: [RenderParam; 4] = [
            RenderParam { kind: MPV_RENDER_PARAM_OPENGL_FBO, data: &mut fbo_data as *mut MpvGlFbo as *mut c_void },
            RenderParam { kind: MPV_RENDER_PARAM_FLIP_Y, data: &mut flip as *mut c_int as *mut c_void },
            RenderParam { kind: MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME, data: &mut block as *mut c_int as *mut c_void },
            RenderParam { kind: 0, data: std::ptr::null_mut() },
        ];
        let r = unsafe { (self.lib.render_context_render)(self.ctx, params.as_ptr()) };
        if r < 0 {
            Err(Error::Mpv(r, error_name(r).into()))
        } else {
            Ok(())
        }
    }
}

impl Drop for RenderContext {
    fn drop(&mut self) {
        unsafe { (self.lib.render_context_free)(self.ctx) };
    }
}

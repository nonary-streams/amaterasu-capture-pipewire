mod token_store;
use super::{Backend, SourceSession};
use crate::{
    buffers::{Mailbox, Plane},
    model::{
        Availability, CaptureResult, PipeWireState, SourceDiagnostics, SourceStage, SourceState,
        SourceType,
    },
};
use std::{
    ffi::{CStr, CString, c_char, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::NonNull,
    sync::Arc,
};
use token_store::TokenStore;

#[derive(Default)]
#[repr(C)]
struct Probe {
    available: u32,
    version: u32,
    source_types: u32,
    cursor_modes: u32,
}
#[derive(Default)]
#[repr(C)]
struct Snapshot {
    state: u32,
    stage: u32,
    node: u32,
    width: u32,
    height: u32,
    format: u32,
    error: u32,
    pipewire_state: i32,
    node_present: u32,
    serial_present: u32,
    pipewire_error: i32,
    serial: u64,
    left: i32,
    top: i32,
    logical_width: u32,
    logical_height: u32,
    desktop_left: i32,
    desktop_top: i32,
    desktop_width: u32,
    desktop_height: u32,
}

#[cfg(amaterasu_kwin)]
unsafe extern "C" {
    fn am_pw_start_output(
        output: *const c_char,
        timeout: u32,
        callback: unsafe extern "C" fn(*mut c_void, *const Plane, u32, u32, u32, u64) -> u32,
        context: *mut c_void,
    ) -> *mut c_void;
}

unsafe extern "C" {
    fn am_pw_probe(output: *mut Probe) -> i32;
    fn am_pw_start(
        source_type: u32,
        timeout: u32,
        restore_token: *const c_char,
        persist: u32,
        token_callback: unsafe extern "C" fn(*mut c_void, *const c_char),
        callback: unsafe extern "C" fn(*mut c_void, *const Plane, u32, u32, u32, u64) -> u32,
        context: *mut c_void,
    ) -> *mut c_void;
    fn am_pw_snapshot(session: *mut c_void, output: *mut Snapshot);
    fn am_pw_destroy(session: *mut c_void);
}

pub struct NativeBackend {
    availability: Availability,
    store: Option<Arc<TokenStore>>,
}

impl NativeBackend {
    pub fn new() -> Self {
        let mut probe = Probe::default();
        // SAFETY: private adapter writes only this live, fixed-layout probe slot.
        unsafe { am_pw_probe(&mut probe) };
        Self {
            store: None,
            availability: Availability {
                native_supported: true,
                available: probe.available != 0,
                portal_version: probe.version,
                source_types: probe.source_types,
                cursor_modes: probe.cursor_modes,
                reason: (probe.available == 0).then_some(
                    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
                        "session_bus_unavailable"
                    } else {
                        "portal_unavailable"
                    },
                ),
            },
        }
    }
    pub fn configure_persistence(&mut self) -> CaptureResult<()> {
        self.store = Some(Arc::new(TokenStore::for_user()?));
        Ok(())
    }
}

impl Backend for NativeBackend {
    fn availability(&self) -> Availability {
        self.availability
    }

    #[cfg(amaterasu_kwin)]
    fn select_output(&self, output: &str, timeout: u32) -> CaptureResult<Box<dyn SourceSession>> {
        let output = CString::new(output).map_err(|_| "invalid_named_output")?;
        let mailbox = Arc::new(Mailbox::default());
        let context = Box::new(CallbackContext {
            mailbox,
            store: None,
            owner: String::new(),
            source: SourceType::Monitor,
        });
        let pointer = (&*context as *const CallbackContext).cast_mut().cast();
        // SAFETY: C copies the output name and retains this boxed callback context
        // only until NativeSession Drop cancels and joins its worker.
        let native = NonNull::new(unsafe {
            am_pw_start_output(output.as_ptr(), timeout, frame_callback, pointer)
        })
        .ok_or("named_capture_unavailable")?;
        Ok(Box::new(NativeSession { native, context }))
    }

    fn can_restore(&self, owner: &str, source: SourceType) -> bool {
        self.availability.portal_version >= 4
            && self
                .store
                .as_ref()
                .is_some_and(|store| store.available(owner, source))
    }
    fn forget(&self, owner: &str, source: SourceType) {
        if let Some(store) = &self.store {
            let _ = store.take(owner, source);
        }
    }
    fn select(
        &self,
        owner: &str,
        source: SourceType,
        timeout: u32,
    ) -> CaptureResult<Box<dyn SourceSession>> {
        let mailbox = Arc::new(Mailbox::default());
        let context = Box::new(CallbackContext {
            mailbox: mailbox.clone(),
            store: self.store.clone(),
            owner: owner.into(),
            source,
        });
        let pointer = (&*context as *const CallbackContext).cast_mut().cast();
        let token = self
            .store
            .as_ref()
            .and_then(|store| store.take(owner, source))
            .and_then(|token| CString::new(token).ok());
        // SAFETY: Arc's allocation has a stable address until session Drop
        // cancels and joins the sole C worker before destroying its mailbox.
        let native = NonNull::new(unsafe {
            am_pw_start(
                source.mask(),
                timeout,
                token
                    .as_ref()
                    .map_or(std::ptr::null(), |token| token.as_ptr()),
                u32::from(self.store.is_some() && self.availability.portal_version >= 4),
                token_callback,
                frame_callback,
                pointer,
            )
        })
        .ok_or("native_worker_unavailable")?;
        Ok(Box::new(NativeSession { native, context }))
    }
}

struct NativeSession {
    native: NonNull<c_void>,
    context: Box<CallbackContext>,
}
struct CallbackContext {
    mailbox: Arc<Mailbox>,
    store: Option<Arc<TokenStore>>,
    owner: String,
    source: SourceType,
}
// SAFETY: adapter snapshots/cancellation use atomics and a thread-safe GCancellable;
// only its dedicated worker accesses GIO/PipeWire resources. Drop joins that worker.
unsafe impl Send for NativeSession {}

impl SourceSession for NativeSession {
    fn geometry(&self) -> Option<amaterasu_media::CaptureGeometry> {
        use amaterasu_media::{CaptureGeometry, DesktopRect};
        let mut snapshot = Snapshot::default();
        // SAFETY: the joined worker retains its handle; the ABI copies one
        // coherent geometry record under its short snapshot lock.
        unsafe { am_pw_snapshot(self.native.as_ptr(), &mut snapshot) };
        CaptureGeometry::new(
            DesktopRect::new(
                snapshot.left,
                snapshot.top,
                snapshot.logical_width,
                snapshot.logical_height,
            )
            .ok()?,
            DesktopRect::new(
                snapshot.desktop_left,
                snapshot.desktop_top,
                snapshot.desktop_width,
                snapshot.desktop_height,
            )
            .ok()?,
        )
        .ok()
    }
    fn state(&self) -> SourceState {
        let mut snapshot = Snapshot::default();
        // SAFETY: this opaque session lives until Drop; output is aligned and writable.
        unsafe { am_pw_snapshot(self.native.as_ptr(), &mut snapshot) };
        match snapshot.state {
            1 => SourceState::Selecting,
            2 if self.context.mailbox.has_frame() => SourceState::Ready {
                node: snapshot.node,
                width: snapshot.width,
                height: snapshot.height,
            },
            2 => SourceState::Selecting,
            3 => SourceState::Denied,
            5 => SourceState::Revoked,
            _ => SourceState::Failed(match snapshot.error {
                1 => "session_bus_unavailable",
                2 => "portal_request_failed",
                3 => "invalid_portal_source",
                4 => "pipewire_failed",
                5 => "unsupported_cpu_buffer",
                6 => "source_mode_changed",
                7 => "source_selection_timeout",
                8 => "kwin_output_capture_failed",
                _ => "capture_failed",
            }),
        }
    }
    fn diagnostics(&self) -> SourceDiagnostics {
        let mut snapshot = Snapshot::default();
        // SAFETY: this opaque session lives until Drop; output is aligned and writable.
        unsafe { am_pw_snapshot(self.native.as_ptr(), &mut snapshot) };
        SourceDiagnostics {
            stage: SourceStage::from_native(snapshot.stage),
            pipewire_node: (snapshot.node_present != 0).then_some(snapshot.node),
            pipewire_serial: (snapshot.serial_present != 0).then_some(snapshot.serial),
            pipewire_state: PipeWireState::from_native(snapshot.pipewire_state),
            pipewire_error: (snapshot.pipewire_error != 0).then_some(snapshot.pipewire_error),
            adapter_error_code: (snapshot.error != 0).then_some(snapshot.error),
        }
    }
    fn mailbox(&self) -> Arc<Mailbox> {
        self.context.mailbox.clone()
    }
}

impl Drop for NativeSession {
    fn drop(&mut self) {
        // SAFETY: uniquely owns this session. Destroy cancels I/O, tears down the
        // portal and PipeWire resources and joins before callback storage is dropped.
        unsafe { am_pw_destroy(self.native.as_ptr()) };
    }
}

unsafe extern "C" fn frame_callback(
    context: *mut c_void,
    plane: *const Plane,
    width: u32,
    height: u32,
    format: u32,
    timestamp_us: u64,
) -> u32 {
    catch_unwind(AssertUnwindSafe(|| {
        if context.is_null() || plane.is_null() {
            return 2;
        }
        // SAFETY: adapter retains the Arc allocation and dequeued mapped SPA buffer
        // through this call. Rust validates the descriptor before copying rows.
        let context = unsafe { &*context.cast::<CallbackContext>() };
        unsafe {
            context
                .mailbox
                .publish(&*plane, width, height, format, timestamp_us)
        }
    }))
    .unwrap_or(2)
}

unsafe extern "C" fn token_callback(context: *mut c_void, token: *const c_char) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if context.is_null() || token.is_null() {
            return;
        }
        // SAFETY: the joined adapter worker lends a valid NUL-terminated token
        // and retains this boxed context until the synchronous callback returns.
        let context = unsafe { &*context.cast::<CallbackContext>() };
        let Ok(token) = (unsafe { CStr::from_ptr(token) }).to_str() else {
            return;
        };
        if let Some(store) = &context.store
            && store.put(&context.owner, context.source, token).is_err()
        {
            eprintln!("amaterasu portal persistence failed code=restore_storage_unavailable");
        }
    }));
}

#[cfg(test)]
mod tests;

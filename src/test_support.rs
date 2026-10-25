//! Private synthetic driver for ownership/state tests. Does not qualify capture.
use crate::{
    backend::{Backend, SourceSession},
    buffers::{Mailbox, Plane},
    controller::Controller,
    model::{Availability, CaptureResult, Settings, SourceDiagnostics, SourceState, SourceType},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

pub struct Driver {
    pub state: Mutex<SourceState>,
    pub diagnostics: Mutex<SourceDiagnostics>,
    pub mailbox: Arc<Mailbox>,
    pub selections: AtomicUsize,
    pub drops: AtomicUsize,
    pub state_reads: AtomicUsize,
    pub revoke_at_read: AtomicUsize,
    pub restore_available: AtomicBool,
}

impl Driver {
    pub fn frame(&self, value: u8, timestamp: u64) {
        let bytes = [value; 16];
        let plane = Plane {
            data: bytes.as_ptr(),
            memory_type: 1,
            flags: 1,
            max_size: 16,
            chunk_offset: 0,
            chunk_size: 16,
            stride: 8,
            chunk_flags: 0,
            plane_count: 1,
            fd: -1,
            map_offset: 0,
        };
        // SAFETY: private driver retains its initialized pixel allocation until copy finishes.
        assert_eq!(
            unsafe { self.mailbox.publish(&plane, 2, 2, 1, timestamp) },
            1
        );
    }
}

pub struct FakeBackend {
    pub driver: Arc<Driver>,
    pub availability: Availability,
}
struct FakeSession(Arc<Driver>);

impl Backend for FakeBackend {
    fn can_restore(&self, _: &str, _: SourceType) -> bool {
        self.driver.restore_available.load(Ordering::SeqCst)
    }
    fn forget(&self, _: &str, _: SourceType) {
        self.driver.restore_available.store(false, Ordering::SeqCst);
    }
    fn select_output(&self, _: &str, _: u32) -> CaptureResult<Box<dyn SourceSession>> {
        self.driver.selections.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeSession(self.driver.clone())))
    }
    fn availability(&self) -> Availability {
        self.availability
    }
    fn select(&self, _: &str, _: SourceType, _: u32) -> CaptureResult<Box<dyn SourceSession>> {
        self.driver.selections.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeSession(self.driver.clone())))
    }
}
impl SourceSession for FakeSession {
    fn state(&self) -> SourceState {
        let read = self.0.state_reads.fetch_add(1, Ordering::SeqCst) + 1;
        if read == self.0.revoke_at_read.load(Ordering::SeqCst) {
            *self.0.state.lock().unwrap() = SourceState::Revoked;
        }
        *self.0.state.lock().unwrap()
    }
    fn diagnostics(&self) -> SourceDiagnostics {
        *self.0.diagnostics.lock().unwrap()
    }
    fn mailbox(&self) -> Arc<Mailbox> {
        self.0.mailbox.clone()
    }
}
impl Drop for FakeSession {
    fn drop(&mut self) {
        self.0.drops.fetch_add(1, Ordering::SeqCst);
    }
}

pub fn setup(settings: Settings) -> (Controller<FakeBackend>, Arc<Driver>) {
    let driver = Arc::new(Driver {
        state: Mutex::new(SourceState::Selecting),
        diagnostics: Mutex::new(SourceDiagnostics::default()),
        mailbox: Arc::new(Mailbox::default()),
        selections: AtomicUsize::new(0),
        drops: AtomicUsize::new(0),
        state_reads: AtomicUsize::new(0),
        revoke_at_read: AtomicUsize::new(usize::MAX),
        restore_available: AtomicBool::new(false),
    });
    let backend = FakeBackend {
        driver: driver.clone(),
        availability: Availability {
            native_supported: true,
            available: true,
            portal_version: 5,
            source_types: 3,
            cursor_modes: 1,
            reason: None,
        },
    };
    (Controller::new(backend, settings), driver)
}

pub fn settings() -> Settings {
    Settings {
        enabled: true,
        selection_principals: ["administrator".into()].into(),
        capture_principals: ["amaterasu.video".into()].into(),
        idle_timeout_seconds: 2,
        selection_timeout_seconds: 5,
        remember_source: false,
        allow_named_outputs: false,
    }
}

use crate::{
    buffers::Mailbox,
    model::{Availability, CaptureResult, SourceDiagnostics, SourceState, SourceType},
};
use std::sync::Arc;

pub trait SourceSession: Send {
    fn geometry(&self) -> Option<amaterasu_media::CaptureGeometry> {
        None
    }
    fn state(&self) -> SourceState;
    fn diagnostics(&self) -> SourceDiagnostics {
        SourceDiagnostics::default()
    }
    fn mailbox(&self) -> Arc<Mailbox>;
}

pub trait Backend: Send {
    fn availability(&self) -> Availability;
    fn select(
        &self,
        owner: &str,
        source: SourceType,
        timeout_seconds: u32,
    ) -> CaptureResult<Box<dyn SourceSession>>;
    fn can_restore(&self, _owner: &str, _source: SourceType) -> bool {
        false
    }
    fn select_output(&self, _output: &str, _timeout: u32) -> CaptureResult<Box<dyn SourceSession>> {
        Err("named_capture_unavailable")
    }
    fn forget(&self, _owner: &str, _source: SourceType) {}
}

#[cfg(amaterasu_pipewire)]
#[path = "platform/mod.rs"]
mod platform;
#[cfg(amaterasu_pipewire)]
pub use platform::NativeBackend;

#[cfg(not(amaterasu_pipewire))]
pub struct NativeBackend;

#[cfg(not(amaterasu_pipewire))]
impl NativeBackend {
    pub fn new() -> Self {
        Self
    }
    pub fn configure_persistence(&mut self) -> CaptureResult<()> {
        Err(unsupported_reason())
    }
}

#[cfg(not(amaterasu_pipewire))]
impl Backend for NativeBackend {
    fn availability(&self) -> Availability {
        Availability::unsupported(unsupported_reason())
    }
    fn select(&self, _: &str, _: SourceType, _: u32) -> CaptureResult<Box<dyn SourceSession>> {
        Err(unsupported_reason())
    }
}

#[cfg(not(amaterasu_pipewire))]
fn unsupported_reason() -> &'static str {
    if cfg!(target_os = "linux") {
        "native_dependencies_unavailable"
    } else {
        "unsupported_platform"
    }
}

//! Current-user ScreenCast portal capture through its restricted PipeWire remote.
//! Native modules are trusted code; runtime API grants are not a native-code sandbox.
mod admission;
mod backend;
mod buffers;
mod controller;
mod model;
mod native;

use amaterasu_module_sdk::{Command, ManagedModule, ModuleResult, Response, RuntimeClient};
use backend::NativeBackend;
use controller::Controller;
use model::{Operation, Settings};
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

const SERVICE: &str = "amaterasu.capture-pipewire/1";

struct CaptureModule {
    runtime: RuntimeClient,
    controller: native::Shared<NativeBackend>,
    _factory: Box<native::Factory<NativeBackend>>,
    epoch: Instant,
    pending_opens: Vec<PendingOpen>,
}
struct PendingOpen {
    request: u64,
    caller: String,
    operation: Operation,
}

impl ManagedModule for CaptureModule {
    fn start(runtime: RuntimeClient) -> ModuleResult<Self> {
        let Response::Result { payload } = runtime.send(Command::GetSettings {})? else {
            return Err("PipeWire settings unavailable".into());
        };
        let settings: Settings =
            serde_json::from_value(payload).map_err(|error| error.to_string())?;
        settings.validate().map_err(str::to_owned)?;
        let mut backend = NativeBackend::new();
        if settings.remember_source {
            backend.configure_persistence().map_err(str::to_owned)?;
        }
        let controller = Arc::new(Mutex::new(Controller::new(backend, settings)));
        let factory = native::Factory::new(controller.clone());
        runtime.require_success(Command::Register {
            service: SERVICE.into(),
        })?;
        // SAFETY: boxed factory remains stable until destruction; every independent
        // binding retains its own shared controller after factory release.
        unsafe { runtime.register_native(SERVICE, &factory.api) }?;
        Ok(Self {
            runtime,
            controller,
            _factory: factory,
            epoch: Instant::now(),
            pending_opens: Vec::new(),
        })
    }

    fn tick(&mut self) -> ModuleResult<()> {
        let now = self.epoch.elapsed().as_millis().min(u64::MAX as u128) as u64;
        self.controller
            .lock()
            .map_err(|_| "PipeWire controller poisoned")?
            .refresh(now);
        let mut cancelled = Vec::new();
        for pending in std::mem::take(&mut self.pending_opens) {
            if matches!(
                self.runtime.send(Command::CheckRequest {
                    request: pending.request
                })?,
                Response::Ok
            ) {
                self.dispatch(pending.request, pending.caller, pending.operation, now)?;
            } else {
                self.controller
                    .lock()
                    .map_err(|_| "PipeWire controller poisoned")?
                    .cancel_pending_open(&pending.caller, &pending.operation);
                cancelled.push(pending.caller);
            }
        }
        for caller in cancelled {
            if !self
                .pending_opens
                .iter()
                .any(|pending| pending.caller == caller)
            {
                self.controller
                    .lock()
                    .map_err(|_| "PipeWire controller poisoned")?
                    .cancel_pending_restore(&caller);
            }
        }
        for _ in 0..8 {
            let (id, caller, service, payload) = match self.runtime.send(Command::Poll {})? {
                Response::Pending => break,
                Response::Request {
                    id,
                    caller,
                    service,
                    payload,
                    ..
                } => (id, caller, service, payload),
                response => return Err(format!("PipeWire poll failed: {response:?}")),
            };
            if !matches!(
                self.runtime.send(Command::CheckRequest { request: id })?,
                Response::Ok
            ) {
                continue;
            }
            if service == SERVICE
                && let Ok(operation) = serde_json::from_value::<Operation>(payload)
            {
                self.dispatch(id, caller, operation, now)?;
            } else {
                self.runtime.send(Command::Complete {request:id,payload:json!({"ok":false,"error":{"code":"invalid_request","message":"invalid_request"}})})?;
            }
        }
        Ok(())
    }
}

impl CaptureModule {
    fn dispatch(
        &mut self,
        id: u64,
        caller: String,
        operation: Operation,
        now: u64,
    ) -> ModuleResult<()> {
        let reply = self
            .controller
            .lock()
            .map_err(|_| "PipeWire controller poisoned")?
            .handle(&caller, operation.clone(), now);
        if matches!(
            reply,
            Err("source_restore_pending" | "named_source_pending")
        ) && self.pending_opens.len() < 8
        {
            self.pending_opens.push(PendingOpen {
                request: id,
                caller,
                operation,
            });
            return Ok(());
        }
        let (payload, created) = match reply {
            Ok(reply) => (reply.payload, reply.created),
            Err(code) => (
                json!({"ok":false,"error":{"code":code,"message":code}}),
                None,
            ),
        };
        let completion = self.runtime.send(Command::Complete {
            request: id,
            payload,
        });
        if !matches!(completion, Ok(Response::Ok)) {
            self.controller
                .lock()
                .map_err(|_| "PipeWire controller poisoned")?
                .rollback(&caller, created);
            if !matches!(completion, Ok(Response::Error { ref code, .. }) if code == "request_ended" || code == "not_found")
            {
                return Err(format!("PipeWire completion failed: {completion:?}"));
            }
        }
        Ok(())
    }
}

impl Drop for CaptureModule {
    fn drop(&mut self) {
        self.controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shutdown();
    }
}
amaterasu_module_sdk::export_module!(CaptureModule);

#[cfg(test)]
mod test_support;

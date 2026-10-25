mod direct;
use crate::{
    admission::Admission,
    backend::{Backend, SourceSession},
    buffers::Mailbox,
    model::{
        CaptureResult, Operation, Settings, SourceDiagnostics, SourceState, SourceType, new_id,
        validate_operation_id,
    },
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::Ordering},
};

const MAX_SELECTIONS: usize = 32;
const MAX_LEASES: u64 = 128;

#[derive(Clone)]
struct SelectionRecord {
    source_type: SourceType,
    output: String,
    state: SourceState,
    diagnostics: SourceDiagnostics,
}

struct Selection {
    owner: String,
    operation: String,
    source: Box<dyn SourceSession>,
    started_at: u64,
    ready_at: Option<u64>,
}

struct Lease {
    id: u64,
    owner: String,
    operation: Option<String>,
    output: String,
    constant_rate: bool,
    admission: Arc<Admission>,
}

#[derive(Clone, Copy)]
pub enum Created {
    Lease(u64),
    Selection,
}
pub struct Reply {
    pub payload: Value,
    pub created: Option<Created>,
}
impl Reply {
    fn existing(payload: Value) -> Self {
        Self {
            payload,
            created: None,
        }
    }
}

pub struct Reader {
    pub mailbox: Arc<Mailbox>,
    pub admission: Arc<Admission>,
    pub geometry: Option<amaterasu_media::CaptureGeometry>,
}

pub struct Controller<B: Backend> {
    backend: B,
    direct: direct::DirectSources,
    settings: Settings,
    selection: Option<Selection>,
    selections: BTreeMap<(String, String), SelectionRecord>,
    last_selection: Option<(String, String)>,
    lease: Option<Lease>,
    closed: BTreeMap<u64, String>,
    opened: u64,
    frames_copied: u64,
    frames_accepted: u64,
}

impl<B: Backend> Controller<B> {
    pub fn new(backend: B, settings: Settings) -> Self {
        Self {
            backend,
            direct: direct::DirectSources::default(),
            settings,
            selection: None,
            selections: BTreeMap::new(),
            last_selection: None,
            lease: None,
            closed: BTreeMap::new(),
            opened: 0,
            frames_copied: 0,
            frames_accepted: 0,
        }
    }

    pub fn handle(&mut self, caller: &str, operation: Operation, now: u64) -> CaptureResult<Reply> {
        self.refresh(now);
        match operation {
            Operation::Status => Ok(Reply::existing(self.status(caller))),
            Operation::Enumerate { offset, limit } => {
                self.enumerate(caller, offset, limit).map(Reply::existing)
            }
            Operation::SelectSource {
                source_type,
                operation_id,
            } => self.select(caller, source_type, operation_id, now),
            Operation::PollSelection { operation_id } => self
                .poll_selection(caller, &operation_id)
                .map(Reply::existing),
            Operation::CancelSelection { operation_id } => {
                validate_operation_id(&operation_id)?;
                self.poll_selection(caller, &operation_id)?;
                if let Some(record) = self.selections.get(&(caller.into(), operation_id.clone())) {
                    self.backend.forget(caller, record.source_type);
                }
                if self.selection.as_ref().is_some_and(|selection| {
                    selection.owner == caller && selection.operation == operation_id
                }) {
                    self.stop_selection(SourceState::Failed("source_selection_cancelled"));
                }
                self.poll_selection(caller, &operation_id)
                    .map(Reply::existing)
            }
            Operation::Open {
                output,
                operation_id,
                constant_rate,
            } => self.open(
                caller,
                output,
                operation_id,
                constant_rate.unwrap_or(false),
                now,
            ),
            Operation::Lookup { operation_id } => {
                validate_operation_id(&operation_id)?;
                if let Some(response) = self.direct.lookup(caller, &operation_id) {
                    return Ok(Reply::existing(response));
                }
                self.lease
                    .as_ref()
                    .filter(|lease| {
                        lease.owner == caller && lease.operation.as_deref() == Some(&operation_id)
                    })
                    .map(lease_reply)
                    .map(Reply::existing)
                    .ok_or("not_found")
            }
            Operation::Close { stream } => {
                self.close(caller, stream)?;
                Ok(Reply::existing(json!({"ok":true})))
            }
        }
    }

    fn select(
        &mut self,
        caller: &str,
        source_type: SourceType,
        operation: String,
        now: u64,
    ) -> CaptureResult<Reply> {
        validate_operation_id(&operation)?;
        if !self.settings.enabled {
            return Err("capture_disabled");
        }
        if !self.settings.selection_principals.contains(caller) {
            return Err("selection_permission_denied");
        }
        let key = (caller.into(), operation.clone());
        if let Some(record) = self.selections.get(&key) {
            if record.source_type != source_type {
                return Err("operation_conflict");
            }
            return Ok(Reply::existing(selection_reply(record, Some(&operation))));
        }
        if self.selection.is_some() || self.lease.is_some() {
            return Err("source_selection_busy");
        }
        if self.selections.len() >= MAX_SELECTIONS {
            return Err("resource_exhausted");
        }
        let availability = self.backend.availability();
        if !availability.available {
            return Err(availability.reason.unwrap_or("portal_unavailable"));
        }
        if availability.source_types & source_type.mask() == 0 {
            return Err("unsupported_source_type");
        }
        let output = format!("portal-{:016x}", new_id()?);
        let source =
            self.backend
                .select(caller, source_type, self.settings.selection_timeout_seconds)?;
        self.selections.insert(
            key.clone(),
            SelectionRecord {
                source_type,
                output,
                state: SourceState::Selecting,
                diagnostics: SourceDiagnostics::default(),
            },
        );
        self.last_selection = Some(key);
        self.selection = Some(Selection {
            owner: caller.into(),
            operation: operation.clone(),
            source,
            started_at: now,
            ready_at: None,
        });
        Ok(Reply {
            payload: self.poll_selection(caller, &operation)?,
            created: Some(Created::Selection),
        })
    }

    fn poll_selection(&self, caller: &str, operation: &str) -> CaptureResult<Value> {
        validate_operation_id(operation)?;
        self.selections
            .get(&(caller.into(), operation.into()))
            .map(|record| selection_reply(record, Some(operation)))
            .ok_or("selection_not_found")
    }

    fn open(
        &mut self,
        caller: &str,
        output: String,
        operation: Option<String>,
        constant_rate: bool,
        now: u64,
    ) -> CaptureResult<Reply> {
        if !self.settings.enabled {
            return Err("capture_disabled");
        }
        if !self.settings.capture_principals.contains(caller) {
            return Err("capture_permission_denied");
        }
        if constant_rate {
            return Err("unsupported_constant_rate");
        }
        if output.starts_with("kwin:") {
            if !self.settings.allow_named_outputs {
                return Err("named_capture_disabled");
            }
            return self.direct.open(
                &self.backend,
                caller,
                output,
                operation,
                now,
                self.settings.selection_timeout_seconds,
            );
        }
        if output == "portal-selected-session-source"
            && self.selection.is_none()
            && self.settings.remember_source
            && self.settings.selection_principals.contains(caller)
        {
            // Reuse one internal operation slot across reconnects. Restoration
            // can only use this caller's previously approved monitor token.
            if let Some(source) = [SourceType::Monitor, SourceType::Window]
                .into_iter()
                .find(|source| self.backend.can_restore(caller, *source))
            {
                let operation = "remembered-source".to_owned();
                self.selections
                    .remove(&(caller.to_owned(), operation.clone()));
                self.select(caller, source, operation, now)?;
            }
        }
        if output == "portal-selected-session-source"
            && self.settings.remember_source
            && self
                .selection
                .as_ref()
                .is_some_and(|selection| selection.owner == caller)
            && self
                .current_record()
                .is_some_and(|record| record.state == SourceState::Selecting)
        {
            return Err("source_restore_pending");
        }
        if let Some(operation) = &operation {
            validate_operation_id(operation)?;
            if let Some(lease) = self.lease.as_ref().filter(|lease| {
                lease.owner == caller && lease.operation.as_ref() == Some(operation)
            }) {
                if lease.output != output || lease.constant_rate != constant_rate {
                    return Err("operation_conflict");
                }
                return Ok(Reply::existing(lease_reply(lease)));
            }
        }
        let record = self
            .current_record()
            .filter(|record| matches!(record.state, SourceState::Ready { .. }))
            .ok_or("source_selection_required")?;
        if record.output != output && output != "portal-selected-session-source" {
            return Err("output_not_found");
        }
        if self.lease.is_some() {
            return Err("resource_exhausted");
        }
        if self.opened >= MAX_LEASES {
            return Err("resource_exhausted");
        }
        let id = new_id()?;
        if self.closed.contains_key(&id) {
            return Err("entropy_error");
        }
        let lease = Lease {
            id,
            owner: caller.into(),
            operation,
            output,
            constant_rate,
            admission: Arc::new(Admission::default()),
        };
        let reply = lease_reply(&lease);
        self.lease = Some(lease);
        self.opened += 1;
        Ok(Reply {
            payload: reply,
            created: Some(Created::Lease(id)),
        })
    }

    pub fn reader(&self, caller: &str, stream: u64) -> CaptureResult<Reader> {
        if let Some(reader) = self.direct.reader(caller, stream) {
            return reader;
        }
        let lease = self
            .lease
            .as_ref()
            .filter(|lease| lease.owner == caller && lease.id == stream)
            .ok_or("not_found")?;
        if lease.admission.is_closed() {
            return Err("not_found");
        }
        let selection = self.selection.as_ref().ok_or("not_found")?;
        if !matches!(selection.source.state(), SourceState::Ready { .. }) {
            return Err("source_unavailable");
        }
        Ok(Reader {
            mailbox: selection.source.mailbox(),
            admission: lease.admission.clone(),
            geometry: selection.source.geometry(),
        })
    }

    pub fn close(&mut self, caller: &str, id: u64) -> CaptureResult<()> {
        if self.direct.close(caller, id) {
            return Ok(());
        }
        if self.closed.get(&id).is_some_and(|owner| owner == caller) {
            return Ok(());
        }
        if !self
            .lease
            .as_ref()
            .is_some_and(|lease| lease.owner == caller && lease.id == id)
        {
            return Err("not_found");
        }
        self.stop_selection(SourceState::Failed("source_lease_closed"));
        Ok(())
    }

    pub fn rollback(&mut self, caller: &str, created: Option<Created>) {
        match created {
            Some(Created::Lease(id)) => {
                let _ = self.close(caller, id);
            }
            Some(Created::Selection)
                if self
                    .selection
                    .as_ref()
                    .is_some_and(|selection| selection.owner == caller) =>
            {
                self.stop_selection(SourceState::Failed("request_ended"));
            }
            _ => {}
        }
    }

    pub fn refresh(&mut self, now: u64) {
        self.direct
            .refresh(now, self.settings.selection_timeout_seconds);
        let Some(selection) = self.selection.as_mut() else {
            return;
        };
        let mut state = selection.source.state();
        let diagnostics = selection.source.diagnostics();
        if matches!(state, SourceState::Ready { .. }) {
            let ready = *selection.ready_at.get_or_insert(now);
            if self.lease.is_none()
                && now.saturating_sub(ready) >= u64::from(self.settings.idle_timeout_seconds) * 1000
            {
                state = SourceState::Failed("source_selection_expired");
            }
        } else if state == SourceState::Selecting
            && now.saturating_sub(selection.started_at)
                >= u64::from(self.settings.selection_timeout_seconds) * 1000
        {
            state = SourceState::Failed("source_selection_timeout");
        }
        let record = self
            .selections
            .get_mut(&(selection.owner.clone(), selection.operation.clone()))
            .expect("selected record");
        record.state = state;
        record.diagnostics = diagnostics;
        if matches!(
            state,
            SourceState::Denied | SourceState::Failed(_) | SourceState::Revoked
        ) {
            self.stop_selection(state);
        }
    }

    pub fn cancel_pending_open(&mut self, caller: &str, operation: &Operation) {
        if let Operation::Open {
            operation_id: Some(operation),
            ..
        } = operation
        {
            self.direct.cancel_pending(caller, operation);
        }
    }
    pub fn cancel_pending_restore(&mut self, caller: &str) {
        if self.lease.is_none()
            && self.selection.as_ref().is_some_and(|selection| {
                selection.owner == caller && selection.operation == "remembered-source"
            })
        {
            self.stop_selection(SourceState::Failed("request_ended"));
        }
    }

    fn stop_selection(&mut self, state: SourceState) {
        if let Some(lease) = self.lease.take() {
            lease.admission.close();
            self.closed.insert(lease.id, lease.owner);
        }
        if let Some(selection) = self.selection.take() {
            self.selections
                .get_mut(&(selection.owner.clone(), selection.operation.clone()))
                .expect("selected record")
                .state = state;
            let mailbox = selection.source.mailbox();
            // SourceSession Drop cancels/joins native work before this returns.
            drop(selection);
            self.frames_copied += mailbox.copied.load(Ordering::Relaxed);
            self.frames_accepted += mailbox.delivered.load(Ordering::Relaxed);
        }
    }

    fn current_record(&self) -> Option<&SelectionRecord> {
        self.selection.as_ref().and_then(|selection| {
            self.selections
                .get(&(selection.owner.clone(), selection.operation.clone()))
        })
    }

    fn enumerate(
        &self,
        caller: &str,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> CaptureResult<Value> {
        if offset.unwrap_or(0) != 0 || !(1..=256).contains(&limit.unwrap_or(256)) {
            return Err("invalid_request");
        }
        if !self.settings.capture_principals.contains(caller) {
            return Err("capture_permission_denied");
        }
        let output = self.current_record().and_then(output_metadata);
        let outputs: Vec<_> = output.into_iter().collect();
        Ok(
            json!({"ok":true,"outputs":outputs,"selection_choices":self.choices(),
            "enumeration_id":self.current_record().map(|record| &record.output),
            "geometry_supported":false}),
        )
    }

    fn choices(&self) -> Vec<SourceType> {
        [SourceType::Monitor, SourceType::Window]
            .into_iter()
            .filter(|source| self.backend.availability().source_types & source.mask() != 0)
            .collect()
    }

    fn status(&self, caller: &str) -> Value {
        let selection = self
            .last_selection
            .as_ref()
            .and_then(|key| self.selections.get(key));
        let phase = if self.selection.is_none()
            && selection.is_some_and(|record| {
                matches!(
                    record.state,
                    SourceState::Failed(
                        "source_lease_closed"
                            | "source_selection_expired"
                            | "source_selection_cancelled"
                            | "capture_shutdown"
                    )
                )
            }) {
            "permission_required"
        } else {
            selection.map_or("permission_required", |record| state_name(record.state))
        };
        let selection_json = self.last_selection.as_ref().and_then(|key| {
            if key.0 == caller {
                selection.map(|record| selection_reply(record, Some(key.1.as_str())))
            } else if self.settings.capture_principals.contains(caller) {
                selection.map(|record| selection_reply(record, None))
            } else {
                None
            }
        });
        let mailbox = self
            .selection
            .as_ref()
            .map(|selection| selection.source.mailbox());
        let accepted = mailbox
            .as_ref()
            .map_or(0, |mailbox| mailbox.delivered.load(Ordering::Relaxed));
        let copied = mailbox
            .as_ref()
            .map_or(0, |mailbox| mailbox.copied.load(Ordering::Relaxed));
        let mut streams: Vec<_> = self
            .lease
            .as_ref()
            .filter(|lease| lease.owner == caller)
            .map(|lease| {
                let mut reply = lease_reply(lease);
                reply["frames_accepted"] = json!(accepted);
                reply
            })
            .into_iter()
            .collect();
        streams.extend(self.direct.streams(caller));
        let availability = self.backend.availability();
        json!({"ok":true,"enabled":self.settings.enabled,"native_supported":availability.native_supported,
            "remember_source":self.settings.remember_source,
            "availability":availability,"selection_state":phase,"selection":selection_json,
            "selection_choices":self.choices(),"streams":streams,"geometry_supported":false,
            "opened":self.opened,"closed":self.closed.len(),"active":usize::from(self.lease.is_some()) + self.direct.active_count(),
            "named_outputs_enabled":self.settings.allow_named_outputs,"named_capture_linked":cfg!(amaterasu_kwin),
            "frames_copied":self.frames_copied + copied,"frames_accepted":self.frames_accepted + accepted})
    }

    pub fn shutdown(&mut self) {
        self.settings.enabled = false;
        self.direct.shutdown();
        self.stop_selection(SourceState::Failed("capture_shutdown"));
    }
}

fn lease_reply(lease: &Lease) -> Value {
    json!({"ok":true,"stream":lease.id,"output":lease.output,"geometry_supported":false})
}

fn state_name(state: SourceState) -> &'static str {
    match state {
        SourceState::Selecting => "selecting",
        SourceState::Ready { .. } => "ready",
        SourceState::Denied => "denied",
        SourceState::Failed(_) | SourceState::Revoked => "failed",
    }
}

fn selection_reply(record: &SelectionRecord, operation: Option<&str>) -> Value {
    json!({"ok":true,"operation_id":operation,"source_type":record.source_type,
        "selection_state":state_name(record.state),"output":output_metadata(record),
        "diagnostics":record.diagnostics,
        "error":match record.state { SourceState::Failed(error) => Some(error), SourceState::Revoked => Some("source_revoked"),
            SourceState::Denied => Some("source_selection_denied"), _ => None }})
}

fn output_metadata(record: &SelectionRecord) -> Option<Value> {
    let SourceState::Ready {
        node,
        width,
        height,
    } = record.state
    else {
        return None;
    };
    Some(
        json!({"id":record.output,"source_type":record.source_type,"width":width,"height":height,
        "format":"bgra8","pipewire_node":node,"geometry_supported":false}),
    )
}

#[cfg(test)]
mod tests;

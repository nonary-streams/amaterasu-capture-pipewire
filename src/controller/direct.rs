//! Named KWin outputs have independent sources and leases; portals stay separate.
use super::{Created, Reader, Reply};
use crate::{
    admission::Admission,
    backend::{Backend, SourceSession},
    model::{CaptureResult, SourceState, new_id, validate_operation_id},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
struct Source {
    owner: String,
    operation: String,
    output: String,
    session: Box<dyn SourceSession>,
    admission: Arc<Admission>,
    id: Option<u64>,
    started: u64,
}
#[derive(Default)]
pub(super) struct DirectSources {
    sources: BTreeMap<(String, String), Source>,
    closed: BTreeMap<u64, String>,
    opened: u64,
}
impl DirectSources {
    pub fn open(
        &mut self,
        backend: &impl Backend,
        caller: &str,
        output: String,
        operation: Option<String>,
        now: u64,
        timeout: u32,
    ) -> CaptureResult<Reply> {
        let Some(name) = output
            .strip_prefix("kwin:")
            .filter(|name| (1..=8).any(|index| *name == format!("Virtual-{index}")))
        else {
            return Err("invalid_named_output");
        };
        let operation = operation.ok_or("operation_id_required")?;
        validate_operation_id(&operation)?;
        let key = (caller.to_owned(), operation.clone());
        if let Some(existing) = self.sources.get(&key) {
            if existing.output != output {
                return Err("operation_conflict");
            }
        } else {
            if self.sources.len() >= 4 || self.opened >= 128 {
                return Err("resource_exhausted");
            }
            let source = backend.select_output(name, timeout)?;
            self.sources.insert(
                key.clone(),
                Source {
                    owner: caller.into(),
                    operation,
                    output,
                    session: source,
                    admission: Arc::new(Admission::default()),
                    id: None,
                    started: now,
                },
            );
        }
        let source = self.sources.get_mut(&key).unwrap();
        match source.session.state() {
            SourceState::Selecting => Err("named_source_pending"),
            SourceState::Ready { .. } => {
                let created = source.id.is_none();
                if created {
                    source.id = Some(new_id()?);
                    self.opened += 1;
                }
                let id = source.id.unwrap();
                Ok(Reply {
                    payload: response(source),
                    created: created.then_some(Created::Lease(id)),
                })
            }
            _ => {
                self.sources.remove(&key);
                Err("named_source_unavailable")
            }
        }
    }
    pub fn lookup(&self, caller: &str, operation: &str) -> Option<Value> {
        self.sources
            .get(&(caller.into(), operation.into()))
            .filter(|source| source.id.is_some())
            .map(response)
    }
    pub fn reader(&self, caller: &str, id: u64) -> Option<CaptureResult<Reader>> {
        let source = self
            .sources
            .values()
            .find(|source| source.owner == caller && source.id == Some(id))?;
        Some(
            if matches!(source.session.state(), SourceState::Ready { .. }) {
                Ok(Reader {
                    mailbox: source.session.mailbox(),
                    admission: source.admission.clone(),
                    geometry: source.session.geometry(),
                })
            } else {
                Err("source_unavailable")
            },
        )
    }
    pub fn close(&mut self, caller: &str, id: u64) -> bool {
        if self.closed.get(&id).is_some_and(|owner| owner == caller) {
            return true;
        }
        let key = self
            .sources
            .iter()
            .find(|(_, source)| source.owner == caller && source.id == Some(id))
            .map(|(key, _)| key.clone());
        if let Some(key) = key {
            if let Some(source) = self.sources.remove(&key) {
                source.admission.close();
                self.closed.insert(id, source.owner.clone());
            }
            true
        } else {
            false
        }
    }
    pub fn cancel_pending(&mut self, caller: &str, operation: &str) {
        let key = (caller.to_owned(), operation.to_owned());
        if self
            .sources
            .get(&key)
            .is_some_and(|source| source.id.is_none())
        {
            self.sources.remove(&key);
        }
    }
    pub fn active_count(&self) -> usize {
        self.sources
            .values()
            .filter(|source| source.id.is_some())
            .count()
    }

    pub fn refresh(&mut self, now: u64, timeout: u32) {
        self.sources.retain(|_, source| {
            source.id.is_some() || now.saturating_sub(source.started) < u64::from(timeout) * 1000
        });
    }
    pub fn streams(&self, caller: &str) -> Vec<Value> {
        self.sources
            .values()
            .filter(|source| source.owner == caller && source.id.is_some())
            .map(response)
            .collect()
    }
    pub fn shutdown(&mut self) {
        for source in self.sources.values() {
            source.admission.close();
        }
        self.sources.clear();
    }
}
fn response(source: &Source) -> Value {
    let mut response = json!({"ok":true,"stream":source.id,"output":source.output,"operation_id":source.operation,"geometry_supported":false});
    if let Some(geometry) = source.session.geometry() {
        response["desktop_geometry"] = json!(geometry);
        response["geometry_supported"] = json!(true);
    }
    response
}

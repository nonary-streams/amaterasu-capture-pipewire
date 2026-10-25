use super::*;
use crate::{
    controller::{Created, Reader},
    model::Operation,
    test_support::{Driver, FakeBackend, settings, setup},
};
use std::sync::atomic::Ordering;

struct TestBinding(*const CaptureApi);
impl TestBinding {
    fn new(factory: &Factory<FakeBackend>, owner: &[u8]) -> Self {
        let mut interface = std::ptr::null();
        let query = NativeInterfaceQuery {
            header: AbiHeader::of::<NativeInterfaceQuery>(),
            interface_major: 1,
            min_minor: 1,
            min_size: size_of::<CaptureApi>() as u32,
        };
        // SAFETY: stable factory, authenticated test caller and aligned query/output last through call.
        assert_eq!(
            unsafe {
                factory.api.bind.unwrap()(
                    factory.api.context,
                    BorrowedBytes::new(owner),
                    &query,
                    &mut interface,
                )
            },
            STATUS_OK
        );
        Self(interface.cast())
    }
    fn api(&self) -> &CaptureApi {
        // SAFETY: wrapper uniquely owns the live boxed binding until Drop.
        unsafe { &*self.0 }
    }
    fn read(&self, id: u64, sink: &mut Sink) -> Status {
        let api = self.api();
        // SAFETY: test binding and exclusive copied sink stay alive through synchronous callback.
        unsafe {
            (api.read_frame)(
                api.base.context,
                id,
                FrameSink {
                    context: (sink as *mut Sink).cast(),
                    write: copy,
                },
            )
        }
    }
}
impl Drop for TestBinding {
    fn drop(&mut self) {
        let api = self.api();
        // SAFETY: uniquely releases this binding once after calls finish.
        unsafe { api.base.release.unwrap()(api.base.context) };
    }
}

fn selected() -> (
    Shared<FakeBackend>,
    Arc<Driver>,
    Box<Factory<FakeBackend>>,
    u64,
) {
    let (mut controller, driver) = setup(settings());
    controller
        .handle(
            "administrator",
            Operation::SelectSource {
                source_type: crate::model::SourceType::Monitor,
                operation_id: "select".into(),
            },
            0,
        )
        .unwrap();
    driver.frame(100, 1000);
    *driver.state.lock().unwrap() = crate::model::SourceState::Ready {
        node: 42,
        width: 2,
        height: 2,
    };
    let output = controller
        .handle(
            "amaterasu.video",
            Operation::Enumerate {
                offset: None,
                limit: None,
            },
            1,
        )
        .unwrap()
        .payload["outputs"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let reply = controller
        .handle(
            "amaterasu.video",
            Operation::Open {
                output,
                operation_id: Some("open".into()),
                constant_rate: None,
            },
            2,
        )
        .unwrap();
    let Some(Created::Lease(id)) = reply.created else {
        panic!("expected owned lease")
    };
    let controller = Arc::new(Mutex::new(controller));
    let factory = Factory::new(controller.clone());
    (controller, driver, factory, id)
}

#[derive(Default)]
struct Sink {
    status: Status,
    frames: Vec<(u64, Vec<u8>)>,
    controller: Option<Shared<FakeBackend>>,
    close: Option<u64>,
}
unsafe extern "C" fn copy(context: *mut c_void, frame: *const CpuFrame) -> Status {
    // SAFETY: native read passes one valid live descriptor and test-owned sink.
    let sink = unsafe { &mut *context.cast::<Sink>() };
    let frame = unsafe { &*frame };
    assert_eq!(frame.validate(), Ok(()));
    assert_eq!(frame.format, PIXEL_BGRA8);
    if let Some(controller) = &sink.controller {
        let mut locked = controller
            .try_lock()
            .expect("foreign callback has no controller lock");
        if let Some(id) = sink.close {
            locked.close("amaterasu.video", id).unwrap();
        }
    }
    // SAFETY: immutable owned pixels survive even callback-triggered source close.
    sink.frames
        .push((frame.sequence, unsafe { frame.bytes() }.unwrap().to_vec()));
    sink.status
}

#[test]
fn factory_binds_before_selection_and_rejects_incompatible_versions_and_size() {
    let (controller, _) = setup(settings());
    let shared = Arc::new(Mutex::new(controller));
    let factory = Factory::new(shared.clone());
    let binding = TestBinding::new(&factory, b"amaterasu.video");
    assert_eq!(binding.read(100, &mut Sink::default()), MEDIA_CLOSED);
    let before = Arc::strong_count(&shared);
    for (major, minor, size) in [
        (2, 1, size_of::<CaptureApi>() as u32),
        (1, 2, size_of::<CaptureApi>() as u32),
        (1, 1, size_of::<CaptureApi>() as u32 + 1),
    ] {
        let query = NativeInterfaceQuery {
            header: AbiHeader::of::<NativeInterfaceQuery>(),
            interface_major: major,
            min_minor: minor,
            min_size: size,
        };
        let mut output = std::ptr::dangling();
        // SAFETY: test retains factory and supplies valid aligned private ABI slots.
        assert_eq!(
            unsafe {
                factory.api.bind.unwrap()(
                    factory.api.context,
                    BorrowedBytes::new(b"caller"),
                    &query,
                    &mut output,
                )
            },
            MEDIA_UNSUPPORTED
        );
        assert!(output.is_null());
        assert_eq!(Arc::strong_count(&shared), before);
    }
}

#[test]
fn binding_copies_principal_and_enforces_owner_geometry_and_closed_handles() {
    let (controller, _, factory, id) = selected();
    let mut caller = b"amaterasu.video".to_vec();
    let binding = TestBinding::new(&factory, &caller);
    caller.fill(b'x');
    drop(caller);
    let foreign = TestBinding::new(&factory, b"other");
    assert_eq!(foreign.read(id, &mut Sink::default()), MEDIA_CLOSED);
    let mut geometry = std::mem::MaybeUninit::<CaptureGeometry>::uninit();
    let api = binding.api();
    // SAFETY: owned binding and aligned writable output exist through call; unsupported writes nothing.
    assert_eq!(
        unsafe { (api.geometry)(api.base.context, id, geometry.as_mut_ptr()) },
        MEDIA_UNSUPPORTED
    );
    assert_eq!(binding.read(id, &mut Sink::default()), STATUS_OK);
    controller
        .lock()
        .unwrap()
        .close("amaterasu.video", id)
        .unwrap();
    assert_eq!(binding.read(id, &mut Sink::default()), MEDIA_CLOSED);
}

#[test]
fn busy_sink_retains_identical_pending_frame_and_native_copy_outlives_provider() {
    let (controller, driver, factory, id) = selected();
    let binding = TestBinding::new(&factory, b"amaterasu.video");
    let mut sink = Sink {
        status: MEDIA_BUSY,
        controller: Some(controller.clone()),
        ..Default::default()
    };
    assert_eq!(binding.read(id, &mut sink), MEDIA_BUSY);
    driver.frame(200, 2000);
    sink.status = STATUS_OK;
    assert_eq!(binding.read(id, &mut sink), STATUS_OK);
    assert_eq!(sink.frames[0], sink.frames[1]);
    assert_eq!(binding.read(id, &mut sink), STATUS_OK);
    assert_eq!(sink.frames[2], (1, vec![200; 16]));
    assert_eq!(binding.read(id, &mut sink), MEDIA_PENDING);
    controller.lock().unwrap().shutdown();
    drop(factory);
    drop(binding);
    assert_eq!(sink.frames[0], (0, vec![100; 16]));
    assert_eq!(driver.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn close_in_callback_keeps_borrow_alive_and_blocks_later_reads() {
    let (controller, driver, factory, id) = selected();
    let binding = TestBinding::new(&factory, b"amaterasu.video");
    let mut sink = Sink {
        controller: Some(controller.clone()),
        close: Some(id),
        ..Default::default()
    };
    assert_eq!(binding.read(id, &mut sink), STATUS_OK);
    assert_eq!(sink.frames[0], (0, vec![100; 16]));
    assert_eq!(binding.read(id, &mut sink), MEDIA_CLOSED);
    assert_eq!(driver.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn per_stream_metrics_are_owned_and_no_controller_lock_is_needed_to_finish_delivery() {
    let (controller, driver, _, id) = selected();
    let Reader { mailbox, .. } = controller
        .lock()
        .unwrap()
        .reader("amaterasu.video", id)
        .unwrap();
    let delivery = mailbox.claim().unwrap().unwrap();
    let locked = controller.lock().unwrap();
    delivery.finish(STATUS_OK);
    drop(locked);
    let owned = controller
        .lock()
        .unwrap()
        .handle("amaterasu.video", Operation::Status, 3)
        .unwrap()
        .payload;
    let foreign = controller
        .lock()
        .unwrap()
        .handle("other", Operation::Status, 3)
        .unwrap()
        .payload;
    assert_eq!(owned["streams"][0]["frames_accepted"], 1);
    assert_eq!(foreign["streams"], serde_json::json!([]));
    assert_eq!(driver.mailbox.delivered.load(Ordering::SeqCst), 1);
}

#[test]
fn a_terminal_source_observed_after_claim_never_starts_a_new_sink_callback() {
    let (_, driver, factory, id) = selected();
    let binding = TestBinding::new(&factory, b"amaterasu.video");
    let reads = driver.state_reads.load(Ordering::SeqCst);
    driver.revoke_at_read.store(reads + 2, Ordering::SeqCst);
    let mut sink = Sink::default();
    assert_eq!(binding.read(id, &mut sink), MEDIA_BACKEND_ERROR);
    assert!(sink.frames.is_empty());
    assert_eq!(driver.mailbox.delivered.load(Ordering::SeqCst), 0);
}

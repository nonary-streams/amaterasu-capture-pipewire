use super::*;
use crate::model::{Availability, PipeWireState, SourceDiagnostics, SourceStage};
use crate::test_support::{Driver, FakeBackend, settings, setup};
use std::sync::atomic::Ordering;

fn select(
    controller: &mut Controller<FakeBackend>,
    owner: &str,
    operation: &str,
    now: u64,
) -> CaptureResult<Reply> {
    controller.handle(
        owner,
        Operation::SelectSource {
            source_type: SourceType::Monitor,
            operation_id: operation.into(),
        },
        now,
    )
}
pub fn ready(controller: &mut Controller<FakeBackend>, driver: &Driver) -> String {
    select(controller, "administrator", "selection", 0).unwrap();
    driver.frame(100, 1000);
    *driver.state.lock().unwrap() = SourceState::Ready {
        node: 42,
        width: 2,
        height: 2,
    };
    controller.refresh(10);
    controller
        .handle(
            "amaterasu.video",
            Operation::Enumerate {
                offset: None,
                limit: None,
            },
            10,
        )
        .unwrap()
        .payload["outputs"][0]["id"]
        .as_str()
        .unwrap()
        .into()
}
pub fn open(
    controller: &mut Controller<FakeBackend>,
    output: &str,
    caller: &str,
    operation: &str,
) -> Reply {
    controller
        .handle(
            caller,
            Operation::Open {
                output: output.into(),
                operation_id: Some(operation.into()),
                constant_rate: None,
            },
            11,
        )
        .unwrap()
}

#[test]
fn unsupported_constant_rate_does_not_allocate_or_replay_a_lease() {
    let (mut controller, driver) = setup(settings());
    let output = ready(&mut controller, &driver);
    let unsupported = || Operation::Open {
        output: output.clone(),
        operation_id: Some("video-1".into()),
        constant_rate: Some(true),
    };
    assert!(matches!(
        controller.handle("amaterasu.video", unsupported(), 11),
        Err("unsupported_constant_rate")
    ));
    assert!(controller.lease.is_none());
    assert_eq!(controller.opened, 0);
    open(&mut controller, &output, "amaterasu.video", "video-1");
    assert!(matches!(
        controller.handle("amaterasu.video", unsupported(), 12),
        Err("unsupported_constant_rate")
    ));
    assert_eq!(controller.opened, 1);
}

#[test]
fn disabled_or_unallowlisted_selection_and_open_never_touch_the_driver() {
    let (mut controller, driver) = setup(Settings::default());
    assert!(matches!(
        select(&mut controller, "administrator", "selection", 0),
        Err("capture_disabled")
    ));
    assert!(matches!(
        controller.handle(
            "amaterasu.video",
            Operation::Open {
                output: "guessed".into(),
                operation_id: None,
                constant_rate: None
            },
            0
        ),
        Err("capture_disabled")
    ));
    controller.settings.enabled = true;
    controller
        .settings
        .capture_principals
        .insert("amaterasu.video".into());
    assert!(matches!(
        select(&mut controller, "administrator", "selection", 0),
        Err("selection_permission_denied")
    ));
    assert!(matches!(
        controller.handle(
            "amaterasu.video",
            Operation::Open {
                output: "guessed".into(),
                operation_id: None,
                constant_rate: None
            },
            0
        ),
        Err("source_selection_required")
    ));
    let status = controller
        .handle("administrator", Operation::Status, 0)
        .unwrap()
        .payload;
    assert_eq!(status["selection_state"], "permission_required");
    assert_eq!(status["availability"]["source_types"], 3);
    assert_eq!(driver.selections.load(Ordering::SeqCst), 0);
}

#[test]
fn enumeration_is_empty_until_actual_ready_and_geometry_is_not_invented() {
    let (mut controller, driver) = setup(settings());
    let initial = controller
        .handle(
            "amaterasu.video",
            Operation::Enumerate {
                offset: None,
                limit: None,
            },
            0,
        )
        .unwrap()
        .payload;
    assert_eq!(initial["outputs"], json!([]));
    assert_eq!(initial["selection_choices"], json!(["monitor", "window"]));
    let output = ready(&mut controller, &driver);
    let page = controller
        .handle(
            "amaterasu.video",
            Operation::Enumerate {
                offset: None,
                limit: Some(1),
            },
            15,
        )
        .unwrap()
        .payload;
    assert_eq!(page["outputs"][0]["id"], output);
    assert_eq!(page["outputs"][0]["width"], 2);
    assert!(page["outputs"][0]["desktop_geometry"].is_null());
    assert_eq!(page["outputs"][0]["geometry_supported"], false);
}

#[test]
fn capture_principals_are_explicit_and_status_does_not_leak_selection_operation_ids() {
    let (mut controller, driver) = setup(settings());
    let output = ready(&mut controller, &driver);

    let denied_enumeration = controller.handle(
        "other",
        Operation::Enumerate {
            offset: None,
            limit: None,
        },
        11,
    );
    assert!(matches!(
        denied_enumeration,
        Err("capture_permission_denied")
    ));
    let denied_open = controller.handle(
        "other",
        Operation::Open {
            output: output.clone(),
            operation_id: Some("other-open".into()),
            constant_rate: None,
        },
        11,
    );
    assert!(matches!(denied_open, Err("capture_permission_denied")));
    assert!(controller.lease.is_none());

    let selection_owner = controller
        .handle("administrator", Operation::Status, 11)
        .unwrap()
        .payload;
    assert_eq!(selection_owner["selection"]["operation_id"], "selection");
    let capture_principal = controller
        .handle("amaterasu.video", Operation::Status, 11)
        .unwrap()
        .payload;
    assert!(capture_principal["selection"]["operation_id"].is_null());
    assert_eq!(capture_principal["selection"]["output"]["id"], output);
    let unrelated = controller
        .handle("other", Operation::Status, 11)
        .unwrap()
        .payload;
    assert!(unrelated["selection"].is_null());
}

#[test]
fn selection_operations_are_owner_scoped_replayed_without_picker_and_cancelled_once() {
    let (mut controller, driver) = setup(settings());
    let first = select(&mut controller, "administrator", "selection", 0).unwrap();
    assert!(matches!(first.created, Some(Created::Selection)));
    assert!(
        select(&mut controller, "administrator", "selection", 1)
            .unwrap()
            .created
            .is_none()
    );
    assert!(matches!(
        controller.handle(
            "administrator",
            Operation::SelectSource {
                source_type: SourceType::Window,
                operation_id: "selection".into()
            },
            1
        ),
        Err("operation_conflict")
    ));
    assert!(matches!(
        controller.handle(
            "other",
            Operation::PollSelection {
                operation_id: "selection".into()
            },
            1
        ),
        Err("selection_not_found")
    ));
    assert!(matches!(
        controller.handle(
            "other",
            Operation::CancelSelection {
                operation_id: "selection".into()
            },
            1
        ),
        Err("selection_not_found")
    ));
    controller
        .handle(
            "administrator",
            Operation::CancelSelection {
                operation_id: "selection".into(),
            },
            2,
        )
        .unwrap();
    controller
        .handle(
            "administrator",
            Operation::CancelSelection {
                operation_id: "selection".into(),
            },
            3,
        )
        .unwrap();
    assert_eq!(driver.selections.load(Ordering::SeqCst), 1);
    assert_eq!(driver.drops.load(Ordering::SeqCst), 1);
    assert!(
        select(&mut controller, "administrator", "selection", 4)
            .unwrap()
            .created
            .is_none()
    );
}

#[test]
fn owned_lease_open_lookup_retry_close_and_failed_completion_cleanup() {
    let (mut controller, driver) = setup(settings());
    let output = ready(&mut controller, &driver);
    let first = open(&mut controller, &output, "amaterasu.video", "open");
    let id = first.payload["stream"].as_u64().unwrap();
    assert_eq!(
        open(&mut controller, &output, "amaterasu.video", "open").payload,
        first.payload
    );
    assert!(
        open(&mut controller, &output, "amaterasu.video", "open")
            .created
            .is_none()
    );
    assert!(controller.reader("other", id).is_err());
    assert!(matches!(
        controller.handle(
            "other",
            Operation::Lookup {
                operation_id: "open".into()
            },
            12
        ),
        Err("not_found")
    ));
    assert!(matches!(
        controller.handle("other", Operation::Close { stream: id }, 12),
        Err("not_found")
    ));
    controller.rollback("amaterasu.video", None);
    assert!(controller.reader("amaterasu.video", id).is_ok());
    controller.rollback("amaterasu.video", first.created);
    controller.close("amaterasu.video", id).unwrap();
    assert!(controller.close("other", id).is_err());
    assert!(controller.reader("amaterasu.video", id).is_err());
    assert_eq!(driver.drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        controller
            .handle("administrator", Operation::Status, 20)
            .unwrap()
            .payload["selection_state"],
        "permission_required"
    );
}

#[test]
fn idle_selection_expires_without_status_extending_its_deadline_and_shutdown_cancels_pending() {
    let (mut controller, driver) = setup(settings());
    ready(&mut controller, &driver);
    controller
        .handle("administrator", Operation::Status, 2009)
        .unwrap();
    assert_eq!(driver.drops.load(Ordering::SeqCst), 0);
    controller.refresh(2010);
    assert_eq!(driver.drops.load(Ordering::SeqCst), 1);
    select(&mut controller, "administrator", "new-selection", 2020).unwrap();
    controller.shutdown();
    controller.shutdown();
    assert_eq!(driver.drops.load(Ordering::SeqCst), 2);
    assert!(controller.selection.is_none());
}

#[test]
fn deny_revoke_and_timeout_release_sessions_and_active_leases() {
    for state in [
        SourceState::Denied,
        SourceState::Failed("native_failure"),
        SourceState::Revoked,
    ] {
        let (mut controller, driver) = setup(settings());
        let output = ready(&mut controller, &driver);
        let id = open(&mut controller, &output, "amaterasu.video", "open").payload["stream"]
            .as_u64()
            .unwrap();
        *driver.state.lock().unwrap() = state;
        controller.refresh(50);
        assert!(controller.reader("amaterasu.video", id).is_err());
        assert!(controller.selection.is_none());
        assert_eq!(driver.drops.load(Ordering::SeqCst), 1);
    }
    let (mut controller, driver) = setup(settings());
    select(&mut controller, "administrator", "selection", 0).unwrap();
    controller.refresh(5000);
    assert_eq!(driver.drops.load(Ordering::SeqCst), 1);
    let result = controller
        .handle(
            "administrator",
            Operation::PollSelection {
                operation_id: "selection".into(),
            },
            5001,
        )
        .unwrap()
        .payload;
    assert_eq!(result["error"], "source_selection_timeout");
}

#[test]
fn timeout_status_distinguishes_waiting_for_start_from_pipewire_connection_failure() {
    let (mut controller, driver) = setup(settings());
    select(&mut controller, "administrator", "start-timeout", 0).unwrap();
    *driver.diagnostics.lock().unwrap() = SourceDiagnostics {
        stage: Some(SourceStage::Start),
        ..SourceDiagnostics::default()
    };
    controller.refresh(5000);
    let timed_out = controller
        .handle(
            "administrator",
            Operation::PollSelection {
                operation_id: "start-timeout".into(),
            },
            5001,
        )
        .unwrap()
        .payload;
    assert_eq!(timed_out["error"], "source_selection_timeout");
    assert_eq!(timed_out["diagnostics"]["stage"], "start");
    assert!(timed_out["diagnostics"].get("pipewire_node").is_none());
    assert!(timed_out["diagnostics"].get("pipewire_serial").is_none());
    assert!(timed_out["diagnostics"].get("pipewire_state").is_none());

    let (mut controller, driver) = setup(settings());
    select(&mut controller, "administrator", "pipewire-error", 0).unwrap();
    *driver.diagnostics.lock().unwrap() = SourceDiagnostics {
        stage: Some(SourceStage::ConnectPipeWire),
        pipewire_node: Some(93),
        pipewire_serial: Some(98),
        pipewire_state: Some(PipeWireState::Error),
        pipewire_error: Some(-2),
        adapter_error_code: Some(4),
    };
    *driver.state.lock().unwrap() = SourceState::Failed("pipewire_failed");
    controller.refresh(10);
    let failed = controller
        .handle(
            "administrator",
            Operation::PollSelection {
                operation_id: "pipewire-error".into(),
            },
            11,
        )
        .unwrap()
        .payload;
    assert_eq!(failed["diagnostics"]["stage"], "connect_pipewire");
    assert_eq!(failed["diagnostics"]["pipewire_node"], 93);
    assert_eq!(failed["diagnostics"]["pipewire_serial"], 98);
    assert_eq!(failed["diagnostics"]["pipewire_state"], "error");
    assert_eq!(failed["diagnostics"]["pipewire_error"], -2);
    assert_eq!(failed["diagnostics"]["adapter_error_code"], 4);
    assert!(failed["diagnostics"].get("session_path").is_none());
    assert!(failed["diagnostics"].get("fd").is_none());
    assert!(failed["diagnostics"].get("token").is_none());
}

#[test]
fn settings_and_selection_bounds_fail_before_native_allocation() {
    assert!(Settings::default().validate().is_ok());
    let mut policy = settings();
    policy.idle_timeout_seconds = 600;
    assert!(policy.validate().is_ok());
    policy.idle_timeout_seconds = 601;
    assert_eq!(policy.validate(), Err("invalid_settings"));
    policy.idle_timeout_seconds = 60;
    policy.selection_timeout_seconds = 301;
    assert_eq!(policy.validate(), Err("invalid_settings"));
    let (mut controller, driver) = setup(settings());
    controller.backend.availability = Availability::unsupported("system_dependencies_unavailable");
    assert!(matches!(
        select(&mut controller, "administrator", "selection", 0),
        Err("system_dependencies_unavailable")
    ));
    assert_eq!(driver.selections.load(Ordering::SeqCst), 0);
    assert!(serde_json::from_value::<Operation>(json!({"action":"select_source","source_type":"monitor","operation_id":"one","caller":"administrator"})).is_err());
    for index in 0..MAX_SELECTIONS {
        controller.backend.availability.available = true;
        controller.backend.availability.source_types = 3;
        let operation = format!("selection-{index}");
        select(&mut controller, "administrator", &operation, 0).unwrap();
        controller
            .handle(
                "administrator",
                Operation::CancelSelection {
                    operation_id: operation,
                },
                0,
            )
            .unwrap();
    }
    assert!(matches!(
        select(&mut controller, "administrator", "too-many", 0),
        Err("resource_exhausted")
    ));
}

#[test]
fn remembered_selection_restores_only_for_allowed_callers_and_waits_for_real_frames() {
    let mut policy = settings();
    policy.remember_source = true;
    policy.selection_principals.insert("amaterasu.video".into());
    let (mut controller, driver) = setup(policy);
    driver.restore_available.store(true, Ordering::SeqCst);
    let request = || Operation::Open {
        output: "portal-selected-session-source".into(),
        operation_id: Some("resume".into()),
        constant_rate: None,
    };
    assert!(matches!(
        controller.handle("stranger", request(), 0),
        Err("capture_permission_denied")
    ));
    assert_eq!(driver.selections.load(Ordering::SeqCst), 0);
    assert!(matches!(
        controller.handle("amaterasu.video", request(), 0),
        Err("source_restore_pending")
    ));
    assert!(controller.lease.is_none());
    assert_eq!(driver.selections.load(Ordering::SeqCst), 1);
    // A repeated pending open must not launch another picker or consume another token.
    assert!(matches!(
        controller.handle("amaterasu.video", request(), 1),
        Err("source_restore_pending")
    ));
    assert_eq!(driver.selections.load(Ordering::SeqCst), 1);
    driver.frame(50, 1000);
    *driver.state.lock().unwrap() = SourceState::Ready {
        node: 42,
        width: 2,
        height: 2,
    };
    let opened = controller.handle("amaterasu.video", request(), 10).unwrap();
    assert!(opened.payload["ok"].as_bool().unwrap());
    let id = opened.payload["stream"].as_u64().unwrap();
    controller.close("amaterasu.video", id).unwrap();
    assert!(controller.lease.is_none());
    assert!(controller.selection.is_none());
    assert_eq!(driver.drops.load(Ordering::SeqCst), 1);
    // Reconnects reuse an internal record instead of exhausting selection history.
    for _ in 0..35 {
        assert!(matches!(
            controller.handle("amaterasu.video", request(), 20),
            Err("source_restore_pending")
        ));
        let opened = controller.handle("amaterasu.video", request(), 21).unwrap();
        controller
            .close(
                "amaterasu.video",
                opened.payload["stream"].as_u64().unwrap(),
            )
            .unwrap();
    }
    assert_eq!(controller.selections.len(), 1);
}

#[test]
fn restoration_is_opt_in_and_cancelled_opens_do_not_leave_capture_running() {
    let (mut controller, driver) = setup(settings());
    driver.restore_available.store(true, Ordering::SeqCst);
    let request = || Operation::Open {
        output: "portal-selected-session-source".into(),
        operation_id: Some("resume".into()),
        constant_rate: None,
    };
    assert!(matches!(
        controller.handle("amaterasu.video", request(), 0),
        Err("source_selection_required")
    ));
    assert_eq!(driver.selections.load(Ordering::SeqCst), 0);
    controller.settings.remember_source = true;
    assert!(matches!(
        controller.handle("amaterasu.video", request(), 0),
        Err("source_selection_required")
    ));
    controller
        .settings
        .selection_principals
        .insert("amaterasu.video".into());
    assert!(matches!(
        controller.handle("amaterasu.video", request(), 0),
        Err("source_restore_pending")
    ));
    controller.cancel_pending_restore("stranger");
    assert_eq!(driver.drops.load(Ordering::SeqCst), 0);
    controller.cancel_pending_restore("amaterasu.video");
    assert_eq!(driver.drops.load(Ordering::SeqCst), 1);
    assert!(controller.selection.is_none());
}

#[test]
fn named_outputs_have_independent_capture_leases_and_cancel_does_not_close_a_peer() {
    let mut settings = crate::test_support::settings();
    settings.allow_named_outputs = true;
    let (mut controller, driver) = crate::test_support::setup(settings);
    let open = |output: &str, operation: &str| Operation::Open {
        output: output.into(),
        operation_id: Some(operation.into()),
        constant_rate: None,
    };
    assert!(matches!(
        controller.handle("amaterasu.video", open("kwin:Virtual-2", "first"), 0),
        Err("named_source_pending")
    ));
    *driver.state.lock().unwrap() = SourceState::Ready {
        node: 1,
        width: 2,
        height: 2,
    };
    driver.frame(42, 1);
    let first = controller
        .handle("amaterasu.video", open("kwin:Virtual-2", "first"), 1)
        .unwrap()
        .payload["stream"]
        .as_u64()
        .unwrap();
    let second = controller
        .handle("amaterasu.video", open("kwin:Virtual-3", "second"), 1)
        .unwrap()
        .payload["stream"]
        .as_u64()
        .unwrap();
    assert_ne!(first, second);
    controller.close("amaterasu.video", first).unwrap();
    assert!(controller.reader("amaterasu.video", first).is_err());
    assert!(controller.reader("amaterasu.video", second).is_ok());
    assert!(controller.reader("other.module", second).is_err());
    controller.cancel_pending_restore("amaterasu.video");
    assert!(controller.reader("amaterasu.video", second).is_ok());
    controller.shutdown();
    assert!(controller.reader("amaterasu.video", second).is_err());
}

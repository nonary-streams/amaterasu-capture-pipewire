use super::*;

fn plane(bytes: &[u8]) -> Plane {
    Plane {
        data: bytes.as_ptr(),
        memory_type: 2,
        flags: 1,
        max_size: bytes.len() as u32,
        chunk_offset: 4,
        chunk_size: 20,
        stride: 12,
        chunk_flags: 0,
        plane_count: 1,
        fd: 55,
        map_offset: 4096,
    }
}

#[test]
fn mapped_memfd_rows_strip_offset_padding_and_normalize_only_bgrx_alpha() {
    let bytes = [
        90, 90, 90, 90, 1, 2, 3, 4, 5, 6, 7, 8, 99, 99, 99, 99, 9, 10, 11, 12, 13, 14, 15, 16,
    ];
    let descriptor = plane(&bytes);
    // SAFETY: test descriptor maps the initialized array through this synchronous copy.
    assert_eq!(
        unsafe { copy_plane(&descriptor, 2, 2, 1) }.unwrap(),
        [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
    );
    // SAFETY: same lifetime and mapping, interpreted as BGRx with unused alpha.
    assert_eq!(
        unsafe { copy_plane(&descriptor, 2, 2, 2) }.unwrap(),
        [1, 2, 3, 255, 5, 6, 7, 255, 9, 10, 11, 255, 13, 14, 15, 255]
    );
    let mut circular_offset = plane(&bytes);
    circular_offset.chunk_offset = circular_offset.max_size + 4;
    // SAFETY: SPA's modulo offset still selects the same complete test-owned rows.
    assert_eq!(
        unsafe { copy_plane(&circular_offset, 2, 2, 1) }.unwrap(),
        [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
    );
}

#[test]
fn rejects_unmapped_dma_planes_unreadable_memory_and_invalid_fd_or_chunks() {
    let bytes = [0u8; 24];
    for mutate in [
        |plane: &mut Plane| plane.data = std::ptr::null(),
        |plane: &mut Plane| plane.memory_type = 3,
        |plane: &mut Plane| plane.flags = 0,
        |plane: &mut Plane| plane.fd = -1,
        |plane: &mut Plane| plane.plane_count = 2,
        |plane: &mut Plane| plane.chunk_offset = u32::MAX,
        |plane: &mut Plane| plane.chunk_size = 21,
        |plane: &mut Plane| plane.chunk_size = 19,
        |plane: &mut Plane| plane.stride = 7,
        |plane: &mut Plane| plane.stride = -12,
        |plane: &mut Plane| plane.chunk_flags = 1,
        |plane: &mut Plane| plane.chunk_flags = 2,
        |plane: &mut Plane| plane.max_size = u32::MAX,
    ] {
        let mut descriptor = plane(&bytes);
        mutate(&mut descriptor);
        // SAFETY: every valid data pointer still references test-owned memory;
        // invalid metadata must be rejected before any pixel dereference.
        assert_eq!(
            unsafe { copy_plane(&descriptor, 2, 2, 1) },
            Err("invalid_frame")
        );
    }
    for (width, height, format) in [
        (0, 2, 1),
        (2, 0, 1),
        (8193, 2, 1),
        (2, 8193, 1),
        (2, 2, 3),
        (u32::MAX, 2, 1),
    ] {
        // SAFETY: descriptor points to valid test memory; invalid dimensions reject before read.
        assert_eq!(
            unsafe { copy_plane(&plane(&bytes), width, height, format) },
            Err("invalid_frame")
        );
    }
}

#[test]
fn rejected_frame_remains_pending_while_latest_real_copy_is_replaced() {
    let mailbox = Arc::new(Mailbox::default());
    let mut bytes = [1u8; 24];
    // SAFETY: byte allocation remains initialized and alive through publish.
    assert_eq!(unsafe { mailbox.publish(&plane(&bytes), 2, 2, 1, 1000) }, 1);
    let delivery = mailbox.claim().unwrap().unwrap();
    let retained = delivery.frame.bytes.clone();
    assert!(matches!(mailbox.claim(), Err(MEDIA_BUSY)));
    bytes.fill(2);
    // SAFETY: mailbox owns an independent copy; updating producer bytes cannot mutate it.
    assert_eq!(unsafe { mailbox.publish(&plane(&bytes), 2, 2, 1, 2000) }, 1);
    delivery.finish(MEDIA_BUSY);
    let retry = mailbox.claim().unwrap().unwrap();
    assert_eq!(retry.frame.sequence, 0);
    assert_eq!(retry.frame.bytes, retained);
    retry.finish(STATUS_OK);
    let next = mailbox.claim().unwrap().unwrap();
    assert_eq!(next.frame.sequence, 1);
    assert_eq!(next.frame.timestamp_us, 1000);
    assert_eq!(next.frame.bytes, [2u8; 16]);
    next.finish(STATUS_OK);
    assert!(mailbox.claim().unwrap().is_none());
    assert!(
        mailbox.has_frame(),
        "initial validated copy remains established after consumption"
    );
    assert_eq!(mailbox.delivered.load(Ordering::SeqCst), 2);
    assert_eq!(mailbox.copied.load(Ordering::SeqCst), 2);
}

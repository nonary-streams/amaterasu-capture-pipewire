use crate::model::CaptureResult;
use amaterasu_media::{MAX_FRAME_BYTES, MEDIA_BUSY};
use amaterasu_module_sdk::abi::{STATUS_OK, Status};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

/// Private adapter descriptor. `data` already points to the beginning of the
/// mapped data region; map_offset describes its FD mapping, not a pixel offset.
#[repr(C)]
pub struct Plane {
    pub data: *const u8,
    pub memory_type: u32,
    pub flags: u32,
    pub max_size: u32,
    pub chunk_offset: u32,
    pub chunk_size: u32,
    pub stride: i32,
    pub chunk_flags: u32,
    pub plane_count: u32,
    pub fd: i64,
    pub map_offset: u32,
}

pub struct OwnedFrame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub bytes: Vec<u8>,
    pub timestamp_us: u64,
    pub sequence: u64,
    accepted: AtomicBool,
}

/// # Safety
/// `plane.data` must address `plane.max_size` bytes mapped readably for this
/// synchronous call. Only validated initialized pixel rows are read; padding
/// is never borrowed. The PipeWire worker retains its dequeued buffer until return.
pub unsafe fn copy_plane(
    plane: &Plane,
    width: u32,
    height: u32,
    format: u32,
) -> CaptureResult<Vec<u8>> {
    let row_bytes = width.checked_mul(4).ok_or("invalid_frame")? as usize;
    let length = row_bytes
        .checked_mul(height as usize)
        .ok_or("invalid_frame")?;
    let stride = usize::try_from(plane.stride).map_err(|_| "invalid_frame")?;
    let source_length = stride
        .checked_mul(height.saturating_sub(1) as usize)
        .and_then(|offset| offset.checked_add(row_bytes))
        .ok_or("invalid_frame")?;
    let maximum = plane.max_size as usize;
    if maximum == 0 {
        return Err("invalid_frame");
    }
    // SPA chunk offsets are circular positions modulo maxsize. Normalize only
    // that position; a pixel row may never wrap across the mapped allocation.
    let offset = plane.chunk_offset as usize % maximum;
    if !matches!(format, 1 | 2)
        || !matches!(plane.memory_type, 1 | 2)
        || plane.flags & 1 == 0
        || (plane.memory_type == 2 && plane.fd < 0)
        || plane.plane_count != 1
        || plane.data.is_null()
        || plane.chunk_flags != 0
        || width == 0
        || height == 0
        || width > 8192
        || height > 8192
        || length > MAX_FRAME_BYTES
        || maximum > MAX_FRAME_BYTES
        || stride < row_bytes
        || offset >= maximum
        || plane.chunk_size as usize > maximum - offset
        || source_length > plane.chunk_size as usize
    {
        return Err("invalid_frame");
    }
    let mut bytes = vec![0; length];
    for row in 0..height as usize {
        // SAFETY: validated chunk, row length and stride bound every readable
        // row inside the live mapped allocation; no padding becomes a slice.
        let source =
            unsafe { std::slice::from_raw_parts(plane.data.add(offset + row * stride), row_bytes) };
        let output = &mut bytes[row * row_bytes..(row + 1) * row_bytes];
        output.copy_from_slice(source);
        if format == 2 {
            for pixel in output.as_chunks_mut::<4>().0 {
                pixel[3] = 255;
            }
        }
    }
    Ok(bytes)
}

#[derive(Default)]
struct Frames {
    latest: Option<Arc<OwnedFrame>>,
    pending: Option<Arc<OwnedFrame>>,
    epoch_us: Option<u64>,
    next_sequence: u64,
}

#[derive(Default)]
pub struct Mailbox {
    frames: Mutex<Frames>,
    in_flight: AtomicBool,
    pub delivered: AtomicU64,
    pub rejected: AtomicU64,
    pub copied: AtomicU64,
    published: AtomicBool,
}

impl Mailbox {
    /// # Safety
    /// The adapter owns plane memory for this synchronous callback only.
    pub unsafe fn publish(
        &self,
        plane: &Plane,
        width: u32,
        height: u32,
        format: u32,
        timestamp_us: u64,
    ) -> u32 {
        let bytes = match unsafe { copy_plane(plane, width, height, format) } {
            Ok(bytes) => bytes,
            Err(_) => {
                self.rejected.fetch_add(1, Ordering::Relaxed);
                return 2;
            }
        };
        let Ok(mut frames) = self.frames.try_lock() else {
            return 0;
        };
        let epoch = *frames.epoch_us.get_or_insert(timestamp_us);
        let sequence = frames.next_sequence;
        frames.next_sequence = frames.next_sequence.saturating_add(1);
        frames.latest = Some(Arc::new(OwnedFrame {
            width,
            height,
            stride: width * 4,
            bytes,
            timestamp_us: timestamp_us.saturating_sub(epoch),
            sequence,
            accepted: AtomicBool::new(false),
        }));
        self.published.store(true, Ordering::SeqCst);
        self.copied.fetch_add(1, Ordering::Relaxed);
        1
    }

    pub fn has_frame(&self) -> bool {
        self.published.load(Ordering::SeqCst)
    }

    pub fn claim(self: &Arc<Self>) -> Result<Option<Delivery>, Status> {
        if self
            .in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(MEDIA_BUSY);
        }
        let mut frames = match self.frames.try_lock() {
            Ok(frames) => frames,
            Err(_) => {
                self.in_flight.store(false, Ordering::SeqCst);
                return Err(MEDIA_BUSY);
            }
        };
        if frames
            .pending
            .as_ref()
            .is_some_and(|frame| frame.accepted.load(Ordering::SeqCst))
        {
            frames.pending = None;
        }
        if frames.pending.is_none() {
            frames.pending = frames.latest.take();
        }
        let frame = frames.pending.clone();
        drop(frames);
        match frame {
            Some(frame) => Ok(Some(Delivery {
                frame,
                mailbox: self.clone(),
            })),
            None => {
                self.in_flight.store(false, Ordering::SeqCst);
                Ok(None)
            }
        }
    }
}

pub struct Delivery {
    pub frame: Arc<OwnedFrame>,
    mailbox: Arc<Mailbox>,
}

impl Delivery {
    pub fn finish(self, status: Status) -> Status {
        if status == STATUS_OK {
            self.frame.accepted.store(true, Ordering::SeqCst);
            self.mailbox.delivered.fetch_add(1, Ordering::SeqCst);
        }
        status
    }
}

impl Drop for Delivery {
    fn drop(&mut self) {
        self.mailbox.in_flight.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests;

use crate::{backend::Backend, controller::Controller};
use amaterasu_media::{
    CAPTURE_INTERFACE_MINOR, CaptureApi, CaptureGeometry, CpuFrame, FrameSink, INTERFACE_MAJOR,
    MEDIA_BACKEND_ERROR, MEDIA_BUSY, MEDIA_CLOSED, MEDIA_PENDING, MEDIA_UNSUPPORTED, PIXEL_BGRA8,
};
use amaterasu_module_sdk::abi::{
    AbiHeader, BorrowedBytes, NativeInterface, NativeInterfaceQuery, NativeServiceFactory,
    STATUS_INTERNAL_ERROR, STATUS_INVALID_ARGUMENT, STATUS_OK, Status,
};
use std::{
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex, TryLockError},
};

pub type Shared<B> = Arc<Mutex<Controller<B>>>;
pub struct Factory<B: Backend> {
    pub api: NativeServiceFactory,
    controller: Shared<B>,
}
impl<B: Backend + 'static> Factory<B> {
    pub fn new(controller: Shared<B>) -> Box<Self> {
        let mut factory = Box::new(Self {
            api: NativeServiceFactory {
                header: AbiHeader::of::<NativeServiceFactory>(),
                context: std::ptr::null_mut(),
                bind: Some(bind::<B>),
            },
            controller,
        });
        factory.api.context = (&mut *factory as *mut Self).cast();
        factory
    }
}

struct Binding<B: Backend> {
    api: CaptureApi,
    controller: Shared<B>,
    caller: String,
}
impl<B: Backend> Binding<B> {
    fn new(controller: Shared<B>, caller: String) -> Box<Self> {
        let mut binding = Box::new(Self {
            api: CaptureApi {
                base: NativeInterface {
                    header: AbiHeader {
                        size: size_of::<CaptureApi>() as u32,
                        major: INTERFACE_MAJOR,
                        minor: CAPTURE_INTERFACE_MINOR,
                    },
                    context: std::ptr::null_mut(),
                    release: Some(release::<B>),
                },
                read_frame: read_frame::<B>,
                geometry: geometry::<B>,
            },
            controller,
            caller,
        });
        binding.api.base.context = (&mut *binding as *mut Self).cast();
        binding
    }
}

unsafe extern "C" fn bind<B: Backend + 'static>(
    context: *mut c_void,
    caller: BorrowedBytes,
    query: *const NativeInterfaceQuery,
    output: *mut *const NativeInterface,
) -> Status {
    guarded(|| {
        if output.is_null()
            || !(output as usize).is_multiple_of(align_of::<*const NativeInterface>())
        {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: broker supplies an aligned writable output slot for this call.
        unsafe { output.write(std::ptr::null()) };
        if context.is_null()
            || query.is_null()
            || !(query as usize).is_multiple_of(align_of::<NativeInterfaceQuery>())
        {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: broker retains the initialized aligned query until return.
        let query = unsafe { &*query };
        if !query.header.supports::<NativeInterfaceQuery>()
            || query.interface_major != INTERFACE_MAJOR
            || query.min_minor > CAPTURE_INTERFACE_MINOR
            || query.min_size as usize > size_of::<CaptureApi>()
        {
            return MEDIA_UNSUPPORTED;
        }
        // SAFETY: authenticated immediate caller bytes last through this synchronous bind.
        let Some(caller) = (unsafe { caller.read() })
            .ok()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .filter(|caller| !caller.is_empty() && caller.len() <= 128)
        else {
            return STATUS_INVALID_ARGUMENT;
        };
        // SAFETY: provider retains this stable boxed factory until all binds stop.
        let factory = unsafe { &*context.cast::<Factory<B>>() };
        let binding = Binding::new(factory.controller.clone(), caller.into());
        let interface = &binding.api.base as *const NativeInterface;
        let _ = Box::into_raw(binding);
        // SAFETY: transfers one stable boxed table, uniquely released by the broker.
        unsafe { output.write(interface) };
        STATUS_OK
    })
}

unsafe extern "C" fn release<B: Backend>(context: *mut c_void) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !context.is_null() {
            // SAFETY: broker uniquely releases the binding after native calls stop.
            unsafe { drop(Box::from_raw(context.cast::<Binding<B>>())) };
        }
    }));
}

unsafe extern "C" fn read_frame<B: Backend>(
    context: *mut c_void,
    stream: u64,
    sink: FrameSink,
) -> Status {
    guarded(|| {
        if context.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: broker retains the native binding until the synchronous call ends.
        let binding = unsafe { &*context.cast::<Binding<B>>() };
        let reader = {
            let controller = match binding.controller.try_lock() {
                Ok(controller) => controller,
                Err(TryLockError::WouldBlock) => return MEDIA_BUSY,
                Err(TryLockError::Poisoned(_)) => return MEDIA_BACKEND_ERROR,
            };
            match controller.reader(&binding.caller, stream) {
                Ok(reader) => reader,
                Err("not_found") => return MEDIA_CLOSED,
                Err(_) => return MEDIA_BACKEND_ERROR,
            }
        };
        if reader.admission.is_closed() {
            return MEDIA_CLOSED;
        }
        let delivery = match reader.mailbox.claim() {
            Ok(Some(delivery)) => delivery,
            Ok(None) => return MEDIA_PENDING,
            Err(status) => return status,
        };
        // This atomic admission point races safely with close: callbacks admitted
        // before close may finish, while close prevents all later callbacks.
        let Some(_admission) = reader.admission.enter() else {
            return MEDIA_CLOSED;
        };
        // Portal revocation or source close can occur between the initial source
        // check and callback admission. Revalidate before entering foreign code.
        {
            let controller = match binding.controller.try_lock() {
                Ok(controller) => controller,
                Err(TryLockError::WouldBlock) => return MEDIA_BUSY,
                Err(TryLockError::Poisoned(_)) => return MEDIA_BACKEND_ERROR,
            };
            match controller.reader(&binding.caller, stream) {
                Ok(_) => {}
                Err("not_found") => return MEDIA_CLOSED,
                Err(_) => return MEDIA_BACKEND_ERROR,
            }
        }
        let pixels = &delivery.frame;
        let frame = CpuFrame {
            width: pixels.width,
            height: pixels.height,
            format: PIXEL_BGRA8,
            stride: pixels.stride,
            data: pixels.bytes.as_ptr(),
            len: pixels.bytes.len(),
            timestamp_us: pixels.timestamp_us,
            sequence: pixels.sequence,
        };
        // SAFETY: an independently owned immutable pixel allocation survives source
        // close and remains borrowed only until this callback returns. No controller
        // or mailbox lock is held in foreign code; consumers must copy retained bytes.
        let status = unsafe { sink.send(&frame) };
        delivery.finish(status)
    })
}

unsafe extern "C" fn geometry<B: Backend>(
    context: *mut c_void,
    stream: u64,
    output: *mut CaptureGeometry,
) -> Status {
    guarded(|| {
        if context.is_null()
            || output.is_null()
            || !(output as usize).is_multiple_of(align_of::<CaptureGeometry>())
        {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: binding context is retained by the broker through this call.
        let binding = unsafe { &*context.cast::<Binding<B>>() };
        let controller = match binding.controller.try_lock() {
            Ok(controller) => controller,
            Err(TryLockError::WouldBlock) => return MEDIA_BUSY,
            Err(TryLockError::Poisoned(_)) => return MEDIA_BACKEND_ERROR,
        };
        match controller.reader(&binding.caller, stream) {
            Ok(reader) => match reader.geometry {
                Some(geometry) => {
                    // SAFETY: the validated aligned output slot is writable for
                    // this synchronous native call and geometry is an owned copy.
                    unsafe { output.write(geometry) };
                    STATUS_OK
                }
                None => MEDIA_UNSUPPORTED,
            },
            Err("not_found") => MEDIA_CLOSED,
            Err(_) => MEDIA_BACKEND_ERROR,
        }
    })
}

fn guarded(operation: impl FnOnce() -> Status) -> Status {
    catch_unwind(AssertUnwindSafe(operation)).unwrap_or(STATUS_INTERNAL_ERROR)
}

#[cfg(test)]
mod tests;

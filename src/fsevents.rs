use std::ffi::{CStr, c_char, c_void};
use std::path::{Path, PathBuf};
use std::ptr;

use crossbeam_channel::{Receiver, unbounded};
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSString, NSURL, NSURLVolumeUUIDStringKey};

use crate::reconciliation::EventBatch;

type FSEventStreamRef = *mut c_void;
type CFRunLoopRef = *mut c_void;

const FILE_EVENTS: u32 = 0x10;
const WATCH_ROOT: u32 = 0x04;
const HISTORY_LOST_FLAGS: u32 = 0x01 | 0x02 | 0x04;
const IDS_WRAPPED: u32 = 0x08;
const ROOT_CHANGED: u32 = 0x20;
const SINCE_NOW: u64 = u64::MAX;

#[repr(C)]
struct FSEventStreamContext {
    version: isize,
    info: *mut c_void,
    retain: Option<unsafe extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<unsafe extern "C" fn(*const c_void)>,
    copy_description: Option<unsafe extern "C" fn(*const c_void) -> *mut c_void>,
}

struct CallbackInfo {
    sender: crossbeam_channel::Sender<EventBatch>,
    stream_identity: String,
}

pub struct EventSource {
    stream: FSEventStreamRef,
    _paths: Retained<NSArray<NSString>>,
    _callback: Box<CallbackInfo>,
}

impl EventSource {
    pub fn start(
        root: &Path,
        stream_identity: String,
        since_event_id: Option<u64>,
        latency_seconds: f64,
    ) -> Result<(Self, Receiver<EventBatch>), String> {
        let (sender, receiver) = unbounded();
        let mut callback = Box::new(CallbackInfo {
            sender,
            stream_identity,
        });
        let root_string = NSString::from_str(&root.to_string_lossy());
        let paths = NSArray::from_retained_slice(&[root_string]);
        let mut context = FSEventStreamContext {
            version: 0,
            info: (&mut *callback as *mut CallbackInfo).cast(),
            retain: None,
            release: None,
            copy_description: None,
        };
        let stream = unsafe {
            FSEventStreamCreate(
                ptr::null(),
                event_callback,
                &mut context,
                (Retained::as_ptr(&paths) as *const c_void).cast_mut(),
                since_event_id.unwrap_or(SINCE_NOW),
                latency_seconds,
                FILE_EVENTS | WATCH_ROOT,
            )
        };
        if stream.is_null() {
            return Err("could not create FSEvents stream".into());
        }
        unsafe {
            FSEventStreamScheduleWithRunLoop(stream, CFRunLoopGetMain(), kCFRunLoopDefaultMode);
            if !FSEventStreamStart(stream) {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                return Err("could not start FSEvents stream".into());
            }
        }
        Ok((
            Self {
                stream,
                _paths: paths,
                _callback: callback,
            },
            receiver,
        ))
    }
}

impl Drop for EventSource {
    fn drop(&mut self) {
        unsafe {
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream);
        }
    }
}

pub fn current_event_id() -> u64 {
    unsafe { FSEventsGetCurrentEventId() }
}

pub fn stream_identity(root: &Path) -> Result<String, String> {
    use std::os::unix::fs::MetadataExt;
    let device = std::fs::metadata(root)
        .map_err(|error| error.to_string())?
        .dev() as libc::dev_t;
    let uuid = unsafe { FSEventsCopyUUIDForDevice(device) };
    if uuid.is_null() {
        return url_volume_identity(root)
            .ok_or_else(|| "FSEvents volume UUID is unavailable".into());
    }
    let string = unsafe { CFUUIDCreateString(ptr::null(), uuid) };
    if string.is_null() {
        unsafe { CFRelease(uuid) };
        return Err("could not format FSEvents volume UUID".into());
    }
    let length = unsafe { CFStringGetLength(string) };
    let capacity = unsafe { CFStringGetMaximumSizeForEncoding(length, 0x0800_0100) } + 1;
    let mut bytes = vec![0_u8; usize::try_from(capacity).map_err(|_| "invalid UUID length")?];
    let copied =
        unsafe { CFStringGetCString(string, bytes.as_mut_ptr().cast(), capacity, 0x0800_0100) };
    unsafe {
        CFRelease(string);
        CFRelease(uuid);
    }
    if !copied {
        return Err("could not decode FSEvents volume UUID".into());
    }
    CStr::from_bytes_until_nul(&bytes)
        .map(|value| value.to_string_lossy().into_owned())
        .map_err(|error| error.to_string())
}

fn url_volume_identity(root: &Path) -> Option<String> {
    let path = NSString::from_str(&root.to_string_lossy());
    let url = NSURL::fileURLWithPath(&path);
    let mut value = None;
    unsafe {
        url.getResourceValue_forKey_error(&mut value, NSURLVolumeUUIDStringKey)
            .ok()?;
    }
    value?
        .downcast::<NSString>()
        .ok()
        .map(|value| value.to_string())
}

unsafe extern "C" fn event_callback(
    _stream: FSEventStreamRef,
    info: *mut c_void,
    count: usize,
    event_paths: *mut c_void,
    event_flags: *const u32,
    event_ids: *const u64,
) {
    if info.is_null() || event_paths.is_null() {
        return;
    }
    let callback = unsafe { &*(info.cast::<CallbackInfo>()) };
    let paths = unsafe { std::slice::from_raw_parts(event_paths.cast::<*const c_char>(), count) };
    let flags = unsafe { std::slice::from_raw_parts(event_flags, count) };
    let ids = unsafe { std::slice::from_raw_parts(event_ids, count) };
    let paths = paths
        .iter()
        .filter(|path| !path.is_null())
        .map(|path| {
            PathBuf::from(
                unsafe { CStr::from_ptr(*path) }
                    .to_string_lossy()
                    .into_owned(),
            )
        })
        .collect();
    let batch = EventBatch {
        stream_identity: callback.stream_identity.clone(),
        highest_event_id: ids.iter().copied().max().unwrap_or_default(),
        paths,
        history_lost: flags.iter().any(|flag| flag & HISTORY_LOST_FLAGS != 0),
        ids_wrapped: flags.iter().any(|flag| flag & IDS_WRAPPED != 0),
        root_changed: flags.iter().any(|flag| flag & ROOT_CHANGED != 0),
    };
    let _ = callback.sender.send(batch);
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        allocator: *const c_void,
        callback: unsafe extern "C" fn(
            FSEventStreamRef,
            *mut c_void,
            usize,
            *mut c_void,
            *const u32,
            *const u64,
        ),
        context: *mut FSEventStreamContext,
        paths_to_watch: *mut c_void,
        since_when: u64,
        latency: f64,
        flags: u32,
    ) -> FSEventStreamRef;
    fn FSEventStreamScheduleWithRunLoop(
        stream: FSEventStreamRef,
        run_loop: CFRunLoopRef,
        mode: *const c_void,
    );
    fn FSEventStreamStart(stream: FSEventStreamRef) -> bool;
    fn FSEventStreamStop(stream: FSEventStreamRef);
    fn FSEventStreamInvalidate(stream: FSEventStreamRef);
    fn FSEventStreamRelease(stream: FSEventStreamRef);
    fn FSEventsGetCurrentEventId() -> u64;
    fn FSEventsCopyUUIDForDevice(device: libc::dev_t) -> *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRunLoopGetMain() -> CFRunLoopRef;
    fn CFUUIDCreateString(allocator: *const c_void, uuid: *const c_void) -> *const c_void;
    fn CFStringGetLength(string: *const c_void) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(
        string: *const c_void,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> bool;
    fn CFRelease(value: *const c_void);
    static kCFRunLoopDefaultMode: *const c_void;
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    #[test]
    fn mounted_root_has_a_persistent_fsevents_uuid() {
        let identity = stream_identity(Path::new("/")).unwrap();
        assert!(!identity.is_empty());
        assert_ne!(
            identity,
            format!("dev:{}", std::fs::metadata("/").unwrap().dev())
        );
    }
}

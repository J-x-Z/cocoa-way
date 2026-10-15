use crate::container_sessions::ProfileId;
use serde_json::json;
use std::collections::HashMap;
use std::ffi::c_void;
use std::io::{ErrorKind, Read};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const SAMPLE_RATE: f64 = 48_000.0;
const CHANNELS: u32 = 2;
const BITS_PER_CHANNEL: u32 = 16;
const FRAMES_PER_BUFFER: usize = 480;
const BUFFER_COUNT: usize = 8;
const BYTES_PER_FRAME: usize = CHANNELS as usize * (BITS_PER_CHANNEL as usize / 8);
const BUFFER_BYTES: usize = FRAMES_PER_BUFFER * BYTES_PER_FRAME;

#[derive(Clone)]
struct AudioStatus {
    session: String,
    state: String,
    detail: String,
    bytes_played: u64,
}

static AUDIO_STATUS: OnceLock<Mutex<HashMap<ProfileId, AudioStatus>>> = OnceLock::new();

fn statuses() -> &'static Mutex<HashMap<ProfileId, AudioStatus>> {
    AUDIO_STATUS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record_status(
    index: ProfileId,
    session: &str,
    state: &str,
    detail: impl Into<String>,
    bytes: u64,
) {
    if let Ok(mut statuses) = statuses().lock() {
        statuses.insert(
            index,
            AudioStatus {
                session: session.into(),
                state: state.into(),
                detail: detail.into(),
                bytes_played: bytes,
            },
        );
    }
}

fn clear_status(index: ProfileId) {
    if let Ok(mut statuses) = statuses().lock() {
        statuses.remove(&index);
    }
}

pub fn snapshot() -> serde_json::Value {
    let sessions = crate::container_sessions::load_sessions();
    let mut entries = statuses()
        .lock()
        .map(|statuses| {
            statuses
                .iter()
                .map(|(index, status)| {
                    json!({
                        "profile_id": index,
                        "session_index": sessions.iter().position(|session| session.id == *index),
                        "session": status.session,
                        "state": status.state,
                        "detail": status.detail,
                        "bytes_played": status.bytes_played,
                        "format": "s16le/48000/2",
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    entries.sort_by_key(|entry| entry["profile_id"].as_str().unwrap_or_default().to_string());
    serde_json::Value::Array(entries)
}

pub struct AudioWorker {
    index: ProfileId,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl AudioWorker {
    pub fn start(index: ProfileId, session: String, socket: String) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        record_status(
            index,
            &session,
            "connecting",
            format!("waiting for {}", socket),
            0,
        );
        let join = thread::Builder::new()
            .name(format!("cocoa-way-audio-{}", index))
            .spawn(move || {
                let result = run_audio_worker(index, &session, &socket, &thread_stop);
                if let Err(error) = result {
                    if !thread_stop.load(Ordering::Relaxed) {
                        log::warn!("Audio forwarding for session #{} stopped: {}", index, error);
                        record_status(index, &session, "error", error, 0);
                    }
                }
            })
            .ok();
        Self { index, stop, join }
    }

    pub fn stop(&mut self) {
        self.request_stop();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        clear_status(self.index);
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for AudioWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_audio_worker(
    index: ProfileId,
    session: &str,
    socket: &str,
    stop: &AtomicBool,
) -> Result<(), String> {
    // An early connection can occupy Apple Container's single pending socket
    // slot before the guest listener is ready. Give the guest relay time to
    // initialize before the first connection attempt.
    let warmup_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < warmup_deadline {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    let deadline = Instant::now() + Duration::from_secs(18);
    let stream = loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let last_error = match UnixStream::connect(socket) {
            Ok(stream) => break stream,
            Err(error) => error.to_string(),
        };
        if Instant::now() >= deadline {
            return Err(format!(
                "timed out connecting to the audio socket: {}",
                last_error
            ));
        }
        thread::sleep(Duration::from_millis(500));
    };
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|error| error.to_string())?;
    record_status(
        index,
        session,
        "idle",
        "connected; waiting for guest playback",
        0,
    );
    play_pcm_stream(index, session, stream, stop)
}

fn play_pcm_stream(
    index: ProfileId,
    session: &str,
    mut stream: UnixStream,
    stop: &AtomicBool,
) -> Result<(), String> {
    let (completed_sender, completed_receiver) = mpsc::channel();
    let mut output = AudioOutput::new(completed_sender)?;
    let mut bytes_played = 0u64;

    for buffer in output.initial_buffers() {
        let length = fill_audio_buffer(&mut stream, buffer, None, stop)?;
        if length == 0 {
            return Ok(());
        }
        output.enqueue(buffer, length)?;
        bytes_played += length as u64;
    }
    output.start()?;
    record_status(
        index,
        session,
        "playing",
        "CoreAudio 48 kHz stereo",
        bytes_played,
    );
    let mut last_status_update = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        let buffer = match completed_receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(buffer) => buffer,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("CoreAudio buffer callback stopped".into());
            }
        };
        let length = fill_audio_buffer(&mut stream, buffer, None, stop)?;
        if length == 0 {
            break;
        }
        output.enqueue(buffer, length)?;
        bytes_played += length as u64;
        if last_status_update.elapsed() >= Duration::from_secs(1) {
            record_status(
                index,
                session,
                "playing",
                "CoreAudio 48 kHz stereo",
                bytes_played,
            );
            last_status_update = Instant::now();
        }
    }
    output.stop();
    Ok(())
}

fn fill_audio_buffer(
    stream: &mut UnixStream,
    buffer: usize,
    prefix: Option<u8>,
    stop: &AtomicBool,
) -> Result<usize, String> {
    let buffer = buffer as AudioQueueBufferRef;
    let mut offset = usize::from(prefix.is_some());
    if let Some(prefix) = prefix {
        unsafe {
            *(*buffer).audio_data.cast::<u8>() = prefix;
        }
    }
    while offset < BUFFER_BYTES && !stop.load(Ordering::Relaxed) {
        let target = unsafe {
            let data = (*buffer).audio_data.cast::<u8>();
            std::slice::from_raw_parts_mut(data.add(offset), BUFFER_BYTES - offset)
        };
        match stream.read(target) {
            Ok(0) => return Ok(offset),
            Ok(length) => offset += length,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::Interrupted | ErrorKind::WouldBlock | ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(format!("audio socket read failed: {}", error)),
        }
    }
    Ok(offset)
}

type OSStatus = i32;
type AudioQueueRef = *mut c_void;
type AudioQueueBufferRef = *mut AudioQueueBuffer;

#[repr(C)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioQueueBuffer {
    audio_data_bytes_capacity: u32,
    audio_data: *mut c_void,
    audio_data_byte_size: u32,
    user_data: *mut c_void,
    packet_description_capacity: u32,
    packet_descriptions: *mut c_void,
    packet_description_count: u32,
}

struct AudioCallbackContext {
    completed: mpsc::Sender<usize>,
}

unsafe extern "C" fn audio_queue_callback(
    user_data: *mut c_void,
    _queue: AudioQueueRef,
    buffer: AudioQueueBufferRef,
) {
    if user_data.is_null() || buffer.is_null() {
        return;
    }
    let context = unsafe { &*(user_data.cast::<AudioCallbackContext>()) };
    let _ = context.completed.send(buffer as usize);
}

struct AudioOutput {
    queue: AudioQueueRef,
    buffers: Vec<usize>,
    callback_context: *mut AudioCallbackContext,
    started: bool,
}

impl AudioOutput {
    fn new(completed: mpsc::Sender<usize>) -> Result<Self, String> {
        let format = AudioStreamBasicDescription {
            sample_rate: SAMPLE_RATE,
            format_id: u32::from_be_bytes(*b"lpcm"),
            format_flags: (1 << 2) | (1 << 3),
            bytes_per_packet: BYTES_PER_FRAME as u32,
            frames_per_packet: 1,
            bytes_per_frame: BYTES_PER_FRAME as u32,
            channels_per_frame: CHANNELS,
            bits_per_channel: BITS_PER_CHANNEL,
            reserved: 0,
        };
        let callback_context = Box::into_raw(Box::new(AudioCallbackContext { completed }));
        let mut queue = std::ptr::null_mut();
        let status = unsafe {
            AudioQueueNewOutput(
                &format,
                Some(audio_queue_callback),
                callback_context.cast(),
                std::ptr::null_mut(),
                std::ptr::null(),
                0,
                &mut queue,
            )
        };
        if status != 0 {
            unsafe { drop(Box::from_raw(callback_context)) };
            return Err(format!(
                "AudioQueueNewOutput failed with OSStatus {}",
                status
            ));
        }

        let mut output = Self {
            queue,
            buffers: Vec::with_capacity(BUFFER_COUNT),
            callback_context,
            started: false,
        };
        for _ in 0..BUFFER_COUNT {
            let mut buffer = std::ptr::null_mut();
            let status =
                unsafe { AudioQueueAllocateBuffer(output.queue, BUFFER_BYTES as u32, &mut buffer) };
            if status != 0 {
                return Err(format!(
                    "AudioQueueAllocateBuffer failed with OSStatus {}",
                    status
                ));
            }
            output.buffers.push(buffer as usize);
        }
        Ok(output)
    }

    fn initial_buffers(&self) -> Vec<usize> {
        self.buffers.clone()
    }

    fn enqueue(&mut self, buffer: usize, length: usize) -> Result<(), String> {
        let buffer = buffer as AudioQueueBufferRef;
        unsafe {
            (*buffer).audio_data_byte_size = length as u32;
        }
        let status = unsafe { AudioQueueEnqueueBuffer(self.queue, buffer, 0, std::ptr::null()) };
        if status == 0 {
            Ok(())
        } else {
            Err(format!(
                "AudioQueueEnqueueBuffer failed with OSStatus {}",
                status
            ))
        }
    }

    fn start(&mut self) -> Result<(), String> {
        let status = unsafe { AudioQueueStart(self.queue, std::ptr::null()) };
        if status == 0 {
            self.started = true;
            Ok(())
        } else {
            Err(format!("AudioQueueStart failed with OSStatus {}", status))
        }
    }

    fn stop(&mut self) {
        if self.started {
            unsafe {
                AudioQueueStop(self.queue, 1);
            }
            self.started = false;
        }
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        self.stop();
        if !self.queue.is_null() {
            unsafe {
                AudioQueueDispose(self.queue, 1);
            }
            self.queue = std::ptr::null_mut();
        }
        if !self.callback_context.is_null() {
            unsafe {
                drop(Box::from_raw(self.callback_context));
            }
            self.callback_context = std::ptr::null_mut();
        }
    }
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioQueueNewOutput(
        format: *const AudioStreamBasicDescription,
        callback: Option<unsafe extern "C" fn(*mut c_void, AudioQueueRef, AudioQueueBufferRef)>,
        user_data: *mut c_void,
        callback_run_loop: *mut c_void,
        callback_run_loop_mode: *const c_void,
        flags: u32,
        queue: *mut AudioQueueRef,
    ) -> OSStatus;
    fn AudioQueueDispose(queue: AudioQueueRef, immediate: u8) -> OSStatus;
    fn AudioQueueAllocateBuffer(
        queue: AudioQueueRef,
        capacity: u32,
        buffer: *mut AudioQueueBufferRef,
    ) -> OSStatus;
    fn AudioQueueEnqueueBuffer(
        queue: AudioQueueRef,
        buffer: AudioQueueBufferRef,
        packet_description_count: u32,
        packet_descriptions: *const c_void,
    ) -> OSStatus;
    fn AudioQueueStart(queue: AudioQueueRef, start_time: *const c_void) -> OSStatus;
    fn AudioQueueStop(queue: AudioQueueRef, immediate: u8) -> OSStatus;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_geometry_is_ten_milliseconds() {
        assert_eq!(BUFFER_BYTES, 1_920);
        assert_eq!(FRAMES_PER_BUFFER as f64 / SAMPLE_RATE, 0.01);
        assert_eq!(BUFFER_COUNT * FRAMES_PER_BUFFER, 3_840);
    }

    #[test]
    fn core_audio_accepts_the_forwarding_format() {
        let (sender, _receiver) = mpsc::channel();
        let mut output = AudioOutput::new(sender).unwrap();
        let buffer = output.initial_buffers()[0];
        unsafe {
            std::ptr::write_bytes(
                (*(buffer as AudioQueueBufferRef)).audio_data,
                0,
                BUFFER_BYTES,
            );
        }
        output.enqueue(buffer, BUFFER_BYTES).unwrap();
    }
}

//! Opt-in host tracing; this module deliberately does not infer causality.
//!
//! Integration (parent owns main/Metal edits):
//! - Add `mod latency_trace;`, then call `LatencyTrace::from_env()` once at startup.
//!   An unset/empty COCOA_WAY_LATENCY_TRACE disables tracing. The value is a JSONL
//!   file path, opened/appended ONLY by the worker. Keep its TraceWriter handle.
//! - Use `record(Stage::HostNativeInput, tags)` and `record(Stage::Forwarded, tags)`
//!   at observation/forwarding, preserving source serial/time and window tags.
//! - Call `next_id()` (allocation-free, process-unique u64) only when enabled to
//!   allocate independent input and render-batch IDs; pass them as Tag::U64.
//!   Reuse an input ID for
//!   SurfaceCommit ONLY with an explicitly verified causal link, never "latest
//!   input"/nearest timestamp/window matching. Otherwise use an independent frame
//!   ID for SurfaceCommit -> ComposeStarted -> CPUSubmitted -> MetalPresented, or
//!   no ID (reported unmatched). Fan-out needs distinct IDs, not duplicate stages.
//!   Multiple commits may share an explicitly tracked pending render-batch ID.
//!   Keep that batch ID through compose/submit/present, then allocate a fresh one.
//!   The analyzer counts ALL coalesced/repeated commits as unmatched to compose;
//!   it never selects the first/latest commit. Compose -> present remains valid.
//! - Record ComposeStarted before drawable acquisition/composition. Capture
//!   `now_ns()` immediately before command-buffer commit for CPUSubmitted and
//!   enqueue it with `record_at_ns`; do not include trace enqueue cost in that time.
//! - Before presentation, register a drawable presented handler ONLY if enabled.
//!   Capture a clone of the trace plus owned/copied tags in that callback, and call
//!   `presented(unsafe { drawable.presentedTime() }, tags)`. Do NOT use callback
//!   time or command-buffer completion as display time. Zero means not presented.
//!   objc2-metal 0.2.2 exposes presentedTime under the enabled MTLDrawable feature;
//!   addPresentedHandler is gated by its currently disabled `block2` feature.
//!   Parent can use `objc2::msg_send![drawable, addPresentedHandler: &*block]` with
//!   the existing block2::RcBlock dependency, without changing Cargo features.
//! - Metal diagnostics share the event stream, host clock and bounded queue.
//!   Analyzers must recognize these auxiliary stages without adding them as
//!   prerequisites to the original causal chain. DrawableAcquireStarted ->
//!   DrawableAcquireFinished brackets nextDrawable, including a nil result.
//!   DrawableAcquireFinished -> ComposeFinished measures CPU-side encoding elapsed
//!   time, including instrumentation overhead, not CPU utilization.
//!   GPUStarted/GPUCompleted use command-buffer API
//!   times; GPUCompletionCallback/MetalPresentedCallback use callback arrival.
//!   Neither callback arrival nor GPU completion is display presentation.
//! - After shutdown cleanup, call TraceWriter::close() to end capture at recording
//!   API admission (NOT at an event timestamp or at the last GPU presentation).
//!   Already-admitted calls finish enqueueing/accounting; later calls return false
//!   without sampling/copying tags. Idle trace/callback clones need not be dropped.
//!   Use finish_timeout(&mut self, budget) for bounded caller waiting, or finish()
//!   ONLY off the UI thread. Both implicitly close. No trace/writer drop joins or
//!   performs file I/O. Idle close detection can take FLUSH_INTERVAL (250 ms).
//! - Ok(Some(summary)) from finish_timeout confirms capture drain and Write::flush,
//!   not fsync/durable storage or completion of GPU callbacks. Ok(None) is timeout,
//!   NOT success: the worker continues and the handle can be retried. Dropping it
//!   detaches, so process exit may still truncate the file. Errors likewise leave
//!   persistence/loss totals unconfirmed. Do not spawn an unobserved finish task.
//! - Stats with capture_counts_final=true cover ONLY calls admitted before close.
//!   Late recording calls are excluded and separately sampled, not queued or
//!   included in dropped/rejected. While ANY producer handle survives, legacy
//!   final=false and lifetime_loss_totals_known=false keep existing analyzers from
//!   claiming final lifetime loss totals. Late callbacks may still be pending,
//!   even if they carry pre-close API timestamps; no summary predicts their count.
//!   Initialization/finish errors are caller owned and also logged by the worker.
//!
//! Times are host CoreAnimation nanoseconds, NOT Unix/Instant/guest time. Source
//! event time is an opaque tag, never a latency timestamp. Guest/calibration work
//! is deferred. Analyze with `python3 scripts/analyze-latency.py TRACE.jsonl`.

use serde::Serialize;
use std::fs::OpenOptions;
use std::io::{self, BufWriter, Write};
use std::sync::Arc;
use std::sync::atomic::{
    AtomicU64, AtomicUsize,
    Ordering::{AcqRel, Acquire, Relaxed, Release},
};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const QUEUE_CAPACITY: usize = 1024;
pub const MAX_TAG_BYTES: usize = 256;
pub const HOST_CLOCK: &str = "host_ca";
const FLUSH_INTERVAL: Duration = Duration::from_millis(250);
const CAPTURE_CLOSED: usize = 1 << (usize::BITS - 1);

/// Unique within this process, independent of input serials/windows/timestamps.
/// File sessions disambiguate process restarts. Exhaustion panics, never wraps.
pub fn next_id() -> u64 {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    NEXT_ID
        .fetch_update(Relaxed, Relaxed, |id| id.checked_add(1))
        .expect("latency correlation IDs exhausted")
}

// Verified in installed objc2-quartz-core 0.2.2: its CACurrentMediaTime wrapper
// requires CABase, NOT enabled in Cargo.toml. Declare the identical C API here.
// https://developer.apple.com/documentation/quartzcore/cacurrentmediatime()
#[link(name = "QuartzCore", kind = "framework")]
unsafe extern "C" {
    fn CACurrentMediaTime() -> f64;
}

/// Host-only clock with the same epoch/units conversion as MTLDrawable.presentedTime.
pub fn now_ns() -> u64 {
    media_time_ns(unsafe { CACurrentMediaTime() }).expect("invalid CoreAnimation clock")
}

/// Convert host media seconds without saturating overflow or accepting NaN.
pub fn media_time_ns(seconds: f64) -> Option<u64> {
    let ns = seconds * 1_000_000_000.0;
    // u64::MAX rounds up to 2^64 in f64; the upper bound must be exclusive.
    (seconds >= 0.0 && ns.is_finite() && ns < u64::MAX as f64).then(|| ns as u64)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Stage {
    HostNativeInput,
    HostInputHandled,
    Forwarded,
    SurfaceCommit,
    ComposeStarted,
    CPUSubmitted,
    MetalPresented,
    DrawableAcquireStarted,
    DrawableAcquireFinished,
    ComposeFinished,
    GPUStarted,
    GPUCompleted,
    GPUCompletionCallback,
    GPUCommandFailed,
    MetalPresentedCallback,
    FrameCallbackDone,
}

/// An observed event is not evidence of protocol forwarding. Only a matching
/// keyboard dispatch currently provides an exact Wayland serial/time join.
pub fn forwarded_keyboard_tags(
    input: u64,
    dispatched: Option<(u64, u32, u32)>,
) -> Option<Tags<'static>> {
    let (id, serial, time) = dispatched.filter(|(id, _, _)| *id == input)?;
    Some(Tags {
        correlation_id: Some(Tag::U64(id)),
        source_event_serial: Some(Tag::U64(u64::from(serial))),
        source_event_time: Some(Tag::U64(u64::from(time))),
        ..Default::default()
    })
}

impl Stage {
    fn requires_reported_time(self) -> bool {
        matches!(
            self,
            Self::MetalPresented | Self::GPUStarted | Self::GPUCompleted
        )
    }
}

/// Borrow tags on the hot path; strings are copied only when tracing is enabled.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(untagged)]
pub enum Tag<'a> {
    String(&'a str),
    U64(u64),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Tags<'a> {
    pub correlation_id: Option<Tag<'a>>,
    pub source_event_serial: Option<Tag<'a>>,
    pub source_event_time: Option<Tag<'a>>,
    pub window_id: Option<Tag<'a>>,
    pub probe_marker: Option<[u8; 120]>,
    pub surface_id: Option<Tag<'a>>,
    /// Cached work at commit handling: bit 0 buffer, bit 1 damage, bit 2 callbacks.
    /// This is not a complete visual-change classifier or raw protocol request log.
    pub commit_flags: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum OwnedTag {
    String(String),
    U64(u64),
}

impl From<Tag<'_>> for OwnedTag {
    fn from(tag: Tag<'_>) -> Self {
        match tag {
            Tag::String(s) => Self::String(s.to_owned()),
            Tag::U64(n) => Self::U64(n),
        }
    }
}

#[derive(Debug, Serialize)]
struct Event {
    stage: Stage,
    timestamp_ns: u64,
    clock: &'static str,
    correlation_id: Option<OwnedTag>,
    source_event_serial: Option<OwnedTag>,
    source_event_time: Option<OwnedTag>,
    window_id: Option<OwnedTag>,
    #[serde(skip_serializing_if = "Option::is_none")]
    probe_marker: Option<Vec<u8>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    surface_id: Option<OwnedTag>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit_flags: Option<u64>,
}

#[derive(Default)]
struct Counts {
    dropped: AtomicU64,
    rejected: AtomicU64,
    late_record_calls: AtomicU64,
    // One atomic linearizes close against admission; low bits count active calls.
    admission: AtomicUsize,
}

impl Counts {
    fn enter(&self) -> Option<Recording<'_>> {
        let mut state = self.admission.load(Relaxed);
        loop {
            if state & CAPTURE_CLOSED != 0 {
                self.late_record_calls.fetch_add(1, Relaxed);
                return None;
            }
            assert!(state < CAPTURE_CLOSED - 1, "too many active trace calls");
            match self
                .admission
                .compare_exchange_weak(state, state + 1, Acquire, Relaxed)
            {
                Ok(_) => return Some(Recording(self)),
                Err(current) => state = current,
            }
        }
    }

    fn close(&self) {
        self.admission.fetch_or(CAPTURE_CLOSED, AcqRel);
    }

    fn closed_and_idle(&self) -> bool {
        self.admission.load(Acquire) == CAPTURE_CLOSED
    }
}

struct Recording<'a>(&'a Counts);

impl Drop for Recording<'_> {
    fn drop(&mut self) {
        // Publish enqueue AND counter updates before the worker freezes totals.
        self.0.admission.fetch_sub(1, Release);
    }
}

struct CloseOnWorkerExit(Arc<Counts>);

impl Drop for CloseOnWorkerExit {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[derive(Clone)]
struct Enabled {
    tx: SyncSender<Event>,
    counts: Arc<Counts>,
}

#[derive(Clone, Default)]
pub struct LatencyTrace {
    inner: Option<Enabled>,
}

/// Returned only after all admitted calls are accounted for, drained and flushed.
/// Counts exclude post-close recording calls and do not measure pending callbacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureSummary {
    pub dropped: u64,
    pub rejected: u64,
    /// Snapshot of recording API calls refused after close, not number of events
    /// or callbacks. A fan-out call counts once; callbacks that never record do not.
    pub late_record_calls_observed: u64,
    /// True only if all producer handles were gone when the drained queue was
    /// observed. False is conservative and never upgrades after returning.
    pub lifetime_loss_totals_known: bool,
}

/// Separately owned; dropping this handle always detaches, never joins or flushes.
pub struct TraceWriter {
    counts: Arc<Counts>,
    _worker: JoinHandle<()>,
    completion: Receiver<io::Result<CaptureSummary>>,
    summary: Option<CaptureSummary>,
}

impl TraceWriter {
    fn spawn(
        counts: Arc<Counts>,
        write: impl FnOnce() -> io::Result<CaptureSummary> + Send + 'static,
    ) -> io::Result<Self> {
        let (tx, completion) = mpsc::sync_channel(1);
        let worker_counts = Arc::clone(&counts);
        let worker = thread::Builder::new()
            .name("latency-trace".into())
            .spawn(move || {
                let close_on_exit = CloseOnWorkerExit(worker_counts);
                let result = write();
                // Also close on sink failure (or via Drop during a panic).
                drop(close_on_exit);
                if let Err(error) = &result {
                    log::warn!("latency trace writer failed: {error}");
                }
                let _ = tx.send(result);
            })?;
        Ok(Self {
            counts,
            _worker: worker,
            completion,
            summary: None,
        })
    }

    /// Idempotent, non-waiting admission cutoff. No clock sampling, I/O or join.
    /// Calls already admitted can enqueue after this returns; new calls cannot.
    pub fn close(&self) {
        self.counts.close();
    }

    /// Close and wait at most `timeout` on the completion channel, NEVER a join.
    /// None means persistence is unconfirmed; retry with this handle if desired.
    /// A blocked sink/admitted producer is not cancelled by timeout. Success is
    /// cached; later calls return the same endpoint summary, not fresh late counts.
    pub fn finish_timeout(&mut self, timeout: Duration) -> io::Result<Option<CaptureSummary>> {
        self.close();
        if let Some(summary) = self.summary {
            return Ok(Some(summary));
        }
        match self.completion.recv_timeout(timeout) {
            Ok(result) => {
                let summary = result?;
                self.summary = Some(summary);
                Ok(Some(summary))
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::other(
                "latency writer exited without a completion result",
            )),
        }
    }

    /// Close, drain, flush and join. Unbounded: ONLY use off the UI thread.
    /// Held idle trace clones do not prevent completion. Returns capture-only
    /// counts, NOT proof that pending callbacks have run or lifetime losses froze.
    #[cfg(test)]
    pub fn finish(self) -> io::Result<CaptureSummary> {
        self.close();
        let result = match self.summary {
            Some(summary) => Ok(summary),
            None => self.completion.recv().unwrap_or_else(|_| {
                Err(io::Error::other(
                    "latency writer exited without a completion result",
                ))
            }),
        };
        self._worker
            .join()
            .map_err(|_| io::Error::other("latency writer panicked"))?;
        result
    }
}

impl LatencyTrace {
    pub fn from_env() -> io::Result<(Self, Option<TraceWriter>)> {
        let Some(path) = std::env::var_os("COCOA_WAY_LATENCY_TRACE").filter(|s| !s.is_empty())
        else {
            return Ok((Self::default(), None));
        };
        let (tx, rx) = mpsc::sync_channel(QUEUE_CAPACITY);
        let counts = Arc::new(Counts::default());
        let worker_counts = Arc::clone(&counts);
        let writer = TraceWriter::spawn(Arc::clone(&counts), move || {
            write_trace(path, rx, worker_counts)
        })?;
        Ok((
            Self {
                inner: Some(Enabled { tx, counts }),
            },
            Some(writer),
        ))
    }

    /// Configuration only; after capture close recording still returns false.
    pub fn enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Full/disconnected queue losses within capture. Invalid times/tags and
    /// post-close calls are separate; this is NOT a lifetime callback-loss total.
    pub fn dropped_count(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |s| s.counts.dropped.load(Relaxed))
    }

    /// Return true only if queued. Disabled tracing neither samples nor allocates.
    pub fn record(&self, stage: Stage, tags: Tags<'_>) -> bool {
        let Some(inner) = &self.inner else {
            return false;
        };
        let Some(_recording) = inner.counts.enter() else {
            return false;
        };
        if stage.requires_reported_time() {
            inner.counts.rejected.fetch_add(1, Relaxed);
            return false;
        }
        Self::enqueue(inner, stage, now_ns(), tags)
    }

    /// Caller-supplied timestamp MUST be in HOST_CLOCK, not source-event/guest time.
    pub fn record_at_ns(&self, stage: Stage, timestamp_ns: u64, tags: Tags<'_>) -> bool {
        let Some(inner) = &self.inner else {
            return false;
        };
        let Some(_recording) = inner.counts.enter() else {
            return false;
        };
        Self::enqueue(inner, stage, timestamp_ns, tags)
    }

    // Caller holds Recording across validation, allocation, enqueue and losses.
    fn enqueue(inner: &Enabled, stage: Stage, timestamp_ns: u64, tags: Tags<'_>) -> bool {
        let values = [
            tags.correlation_id,
            tags.source_event_serial,
            tags.source_event_time,
            tags.window_id,
            tags.surface_id,
        ];
        if (stage.requires_reported_time() && timestamp_ns == 0)
            || values
                .into_iter()
                .flatten()
                .any(|tag| matches!(tag, Tag::String(s) if s.is_empty() || s.len() > MAX_TAG_BYTES))
        {
            inner.counts.rejected.fetch_add(1, Relaxed);
            return false;
        }
        let event = Event {
            stage,
            timestamp_ns,
            clock: HOST_CLOCK,
            correlation_id: tags.correlation_id.map(Into::into),
            source_event_serial: tags.source_event_serial.map(Into::into),
            source_event_time: tags.source_event_time.map(Into::into),
            window_id: tags.window_id.map(Into::into),
            probe_marker: tags.probe_marker.map(|marker| marker.to_vec()),
            surface_id: tags.surface_id.map(Into::into),
            commit_flags: tags.commit_flags,
        };
        if inner.tx.try_send(event).is_err() {
            inner.counts.dropped.fetch_add(1, Relaxed);
            return false;
        }
        true
    }

    /// Fan out one sampled milestone without sampling a different time per ID.
    pub fn record_frame_at_ns(
        &self,
        stage: Stage,
        timestamp_ns: u64,
        frames: &[u64],
        window: u64,
    ) -> usize {
        let Some(inner) = &self.inner else {
            return 0;
        };
        let Some(_recording) = inner.counts.enter() else {
            return 0;
        };
        let mut queued = 0;
        for &id in frames {
            queued += usize::from(Self::enqueue(
                inner,
                stage,
                timestamp_ns,
                Tags {
                    correlation_id: Some(Tag::U64(id)),
                    window_id: Some(Tag::U64(window)),
                    ..Default::default()
                },
            ));
        }
        queued
    }

    /// Call only for successful command-buffer completion, using its GPU API times.
    /// Reject unavailable times individually and reversed pairs as a whole.
    /// Never replace an API timestamp with callback time.
    pub fn gpu_execution(&self, started: f64, completed: f64, tags: Tags<'_>) -> bool {
        let Some(inner) = &self.inner else {
            return false;
        };
        let Some(_recording) = inner.counts.enter() else {
            return false;
        };
        let start = media_time_ns(started).filter(|ns| *ns > 0);
        let end = media_time_ns(completed).filter(|ns| *ns > 0);
        if start
            .zip(end)
            .is_some_and(|(start, end)| end < start || completed < started)
        {
            inner.counts.rejected.fetch_add(1, Relaxed);
            return false;
        }
        let record_time = |stage, time| match time {
            Some(ns) => Self::enqueue(inner, stage, ns, tags),
            None => {
                inner.counts.rejected.fetch_add(1, Relaxed);
                false
            }
        };
        let start_queued = record_time(Stage::GPUStarted, start);
        let end_queued = record_time(Stage::GPUCompleted, end);
        start_queued && end_queued
    }

    /// Pass actual drawable.presentedTime seconds, not the callback's arrival time.
    /// https://developer.apple.com/documentation/metal/mtldrawable/presentedtime
    pub fn presented(&self, seconds: f64, tags: Tags<'_>) -> bool {
        let Some(inner) = &self.inner else {
            return false;
        };
        let Some(_recording) = inner.counts.enter() else {
            return false;
        };
        match media_time_ns(seconds).filter(|ns| *ns > 0) {
            Some(ns) => Self::enqueue(inner, Stage::MetalPresented, ns, tags),
            None => {
                inner.counts.rejected.fetch_add(1, Relaxed);
                false
            }
        }
    }

    /// Explicit fast constructor: no environment mutation, clock, disk, or thread.
    #[cfg(test)]
    fn fast_for_test(capacity: usize) -> (Self, Receiver<Event>) {
        let (tx, rx) = mpsc::sync_channel(capacity);
        (
            Self {
                inner: Some(Enabled {
                    tx,
                    counts: Arc::new(Counts::default()),
                }),
            },
            rx,
        )
    }
}

fn json_line(out: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *out, value).map_err(io::Error::other)?;
    out.write_all(b"\n")
}

fn write_trace(
    path: std::ffi::OsString,
    rx: Receiver<Event>,
    counts: Arc<Counts>,
) -> io::Result<CaptureSummary> {
    let session = format!("{}-{}-{}", std::process::id(), now_ns(), next_id());
    let mut out = BufWriter::new(OpenOptions::new().create(true).append(true).open(path)?);
    write_trace_to(&mut out, &session, rx, &counts)
}

fn write_trace_to(
    out: &mut impl Write,
    session: &str,
    rx: Receiver<Event>,
    counts: &Counts,
) -> io::Result<CaptureSummary> {
    json_line(
        out,
        &serde_json::json!({
            "schema": 1, "kind": "session", "session_id": session,
            "clock": HOST_CLOCK, "queue_capacity": QUEUE_CAPACITY,
            "capture_endpoint": "recording_api_admission_not_event_timestamp",
            "late_callbacks": "post_close_recording_calls_excluded_from_capture",
        }),
    )?;
    #[derive(Serialize)]
    struct Row<'a> {
        schema: u8,
        kind: &'static str,
        session_id: &'a str,
        #[serde(flatten)]
        event: &'a Event,
    }
    let mut last_flush = Instant::now();
    loop {
        // Once closed AND quiescent, no producer can enqueue or alter capture
        // counters again. Channel emptiness alone is insufficient before that.
        let (event, capture_drained, producers_gone) = if counts.closed_and_idle() {
            match rx.try_recv() {
                Ok(event) => (Some(event), false, false),
                Err(mpsc::TryRecvError::Empty) => (None, true, false),
                Err(mpsc::TryRecvError::Disconnected) => (None, true, true),
            }
        } else {
            match rx.recv_timeout(FLUSH_INTERVAL) {
                Ok(event) => (Some(event), false, false),
                Err(mpsc::RecvTimeoutError::Timeout) => (None, false, false),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    counts.close();
                    continue;
                }
            }
        };
        if let Some(event) = event {
            json_line(
                out,
                &Row {
                    schema: 1,
                    kind: "event",
                    session_id: session,
                    event: &event,
                },
            )?;
        }
        if capture_drained || last_flush.elapsed() >= FLUSH_INTERVAL {
            let summary = CaptureSummary {
                dropped: counts.dropped.load(Relaxed),
                rejected: counts.rejected.load(Relaxed),
                late_record_calls_observed: counts.late_record_calls.load(Relaxed),
                lifetime_loss_totals_known: capture_drained && producers_gone,
            };
            json_line(
                out,
                &serde_json::json!({
                    "schema": 1, "kind": "stats", "session_id": session,
                    "dropped": summary.dropped, "rejected": summary.rejected,
                    "final": summary.lifetime_loss_totals_known,
                    "capture_counts_final": capture_drained,
                    "capture_drained": capture_drained,
                    "late_record_calls_observed": summary.late_record_calls_observed,
                    "lifetime_loss_totals_known": summary.lifetime_loss_totals_known,
                }),
            )?;
            out.flush()?;
            if capture_drained {
                return Ok(summary);
            }
            last_flush = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Barrier, Mutex};

    const TEST_WAIT: Duration = Duration::from_secs(5);

    #[derive(Clone, Default)]
    struct MemorySink(Arc<Mutex<Vec<u8>>>);

    impl Write for MemorySink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl MemorySink {
        fn rows(&self) -> Vec<serde_json::Value> {
            let bytes = self.0.lock().unwrap();
            assert_eq!(bytes.last(), Some(&b'\n'));
            std::str::from_utf8(&bytes)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    fn writer_for_test(sink: impl Write + Send + 'static) -> (LatencyTrace, TraceWriter) {
        let (trace, rx) = LatencyTrace::fast_for_test(8);
        let counts = Arc::clone(&trace.inner.as_ref().unwrap().counts);
        let worker_counts = Arc::clone(&counts);
        let writer = TraceWriter::spawn(counts, move || {
            let mut sink = sink;
            write_trace_to(&mut sink, "test-session", rx, &worker_counts)
        })
        .unwrap();
        (trace, writer)
    }

    #[test]
    fn close_drains_with_held_clones_and_discloses_late_calls() {
        let sink = MemorySink::default();
        let (trace, mut writer) = writer_for_test(sink.clone());
        let idle_callback = trace.clone();
        assert!(trace.record_at_ns(Stage::Forwarded, 1, Tags::default()));
        assert!(!trace.presented(0.0, Tags::default()));
        writer.close();
        writer.close();
        // Every public recording entry point refuses admission before validation,
        // sampling the clock, allocating owned tags, or enqueueing anything.
        assert!(!idle_callback.record(Stage::Forwarded, Tags::default()));
        assert!(!idle_callback.record_at_ns(Stage::Forwarded, 1, Tags::default()));
        assert_eq!(
            idle_callback.record_frame_at_ns(Stage::Forwarded, 1, &[1, 2], 3),
            0
        );
        assert!(!idle_callback.gpu_execution(f64::NAN, f64::NAN, Tags::default()));
        assert!(!idle_callback.presented(0.0, Tags::default()));
        let summary = writer.finish_timeout(TEST_WAIT).unwrap().unwrap();
        assert_eq!(summary.dropped, 0);
        assert_eq!(summary.rejected, 1);
        // The worker may have finalized before some/all of the late calls.
        assert!(summary.late_record_calls_observed <= 5);
        assert!(!summary.lifetime_loss_totals_known);
        let rows = sink.rows();
        assert_eq!(rows.iter().filter(|row| row["kind"] == "event").count(), 1);
        assert_eq!(
            rows[0]["capture_endpoint"],
            "recording_api_admission_not_event_timestamp"
        );
        let stats = rows.last().unwrap();
        assert_eq!(stats["kind"], "stats");
        assert_eq!(stats["capture_counts_final"], true);
        assert_eq!(stats["capture_drained"], true);
        assert_eq!(stats["final"], false);
        assert_eq!(stats["lifetime_loss_totals_known"], false);
        assert!(!idle_callback.record_at_ns(Stage::Forwarded, 1, Tags::default()));
        assert_eq!(
            trace
                .inner
                .as_ref()
                .unwrap()
                .counts
                .late_record_calls
                .load(Relaxed),
            6
        );
        assert_eq!(
            writer.finish_timeout(Duration::ZERO).unwrap(),
            Some(summary)
        );
        assert_eq!(sink.rows(), rows);
    }

    #[test]
    fn disconnected_producers_allow_final_lifetime_counts() {
        let sink = MemorySink::default();
        let (trace, writer) = writer_for_test(sink.clone());
        assert!(trace.record_at_ns(Stage::Forwarded, 1, Tags::default()));
        drop(trace);
        let summary = writer.finish().unwrap();
        assert!(summary.lifetime_loss_totals_known);
        assert_eq!(summary.late_record_calls_observed, 0);
        assert_eq!(sink.rows().last().unwrap()["final"], true);
    }

    #[test]
    fn close_waits_for_admitted_enqueue_and_counter_updates_not_idle_clones() {
        let sink = MemorySink::default();
        let (trace, mut writer) = writer_for_test(sink.clone());
        let inner = trace.inner.as_ref().unwrap();
        // Deterministically suspend one admitted call across the close boundary.
        let recording = inner.counts.enter().unwrap();
        writer.close();
        assert_eq!(writer.finish_timeout(Duration::ZERO).unwrap(), None);
        assert!(LatencyTrace::enqueue(
            inner,
            Stage::Forwarded,
            1,
            Tags::default()
        ));
        assert!(!LatencyTrace::enqueue(
            inner,
            Stage::MetalPresented,
            0,
            Tags::default()
        ));
        assert_eq!(writer.finish_timeout(Duration::ZERO).unwrap(), None);
        drop(recording);
        let summary = writer.finish_timeout(TEST_WAIT).unwrap().unwrap();
        assert_eq!(summary.dropped, 0);
        assert_eq!(summary.rejected, 1);
        assert_eq!(
            sink.rows()
                .iter()
                .filter(|row| row["kind"] == "event")
                .count(),
            1
        );
    }

    #[test]
    fn concurrent_record_and_close_account_for_every_attempt() {
        let sink = MemorySink::default();
        let (trace, mut writer) = writer_for_test(sink.clone());
        assert!(trace.record_at_ns(Stage::Forwarded, 1, Tags::default()));
        let start = Arc::new(Barrier::new(5));
        let mut producers = Vec::new();
        for _ in 0..4 {
            let trace = trace.clone();
            let start = Arc::clone(&start);
            producers.push(thread::spawn(move || {
                start.wait();
                (0..1000)
                    .filter(|_| trace.record_at_ns(Stage::Forwarded, 1, Tags::default()))
                    .count()
            }));
        }
        start.wait();
        writer.close();
        let summary = writer.finish_timeout(TEST_WAIT).unwrap().unwrap();
        let accepted = 1 + producers
            .into_iter()
            .map(|p| p.join().unwrap())
            .sum::<usize>();
        let late = trace
            .inner
            .as_ref()
            .unwrap()
            .counts
            .late_record_calls
            .load(Relaxed);
        assert_eq!(accepted as u64 + summary.dropped + late, 4001);
        assert_eq!(summary.rejected, 0);
        assert!(summary.late_record_calls_observed <= late);
        assert!(!summary.lifetime_loss_totals_known);
        let rows = sink.rows();
        assert_eq!(
            rows.iter().filter(|row| row["kind"] == "event").count(),
            accepted
        );
        assert_eq!(rows.last().unwrap()["capture_drained"], true);
        assert_eq!(rows.last().unwrap()["dropped"], summary.dropped);
    }

    struct FailingSink {
        bytes: MemorySink,
        fail_flush: bool,
    }

    impl Write for FailingSink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if !self.fail_flush {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "test write failure",
                ));
            }
            self.bytes.write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "test flush failure",
            ))
        }
    }

    #[test]
    fn sink_write_failure_closes_admission_without_success_or_final_stats() {
        let bytes = MemorySink::default();
        let (trace, mut writer) = writer_for_test(FailingSink {
            bytes: bytes.clone(),
            fail_flush: false,
        });
        // Observe worker failure without calling close/finish first: the worker
        // itself must close admission, even when no shutdown caller is running.
        let error = writer
            .completion
            .recv_timeout(TEST_WAIT)
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("test write failure"));
        assert!(!trace.record_at_ns(Stage::Forwarded, 1, Tags::default()));
        assert!(writer.finish_timeout(Duration::ZERO).is_err());
        assert!(writer.summary.is_none());
        assert!(bytes.0.lock().unwrap().is_empty());
    }

    #[test]
    fn worker_panic_closes_admission_and_never_returns_a_success_summary() {
        let (trace, rx) = LatencyTrace::fast_for_test(1);
        let counts = Arc::clone(&trace.inner.as_ref().unwrap().counts);
        let writer = TraceWriter::spawn(counts, move || {
            drop(rx);
            panic!("test writer panic");
        })
        .unwrap();
        assert!(matches!(
            writer.completion.recv_timeout(TEST_WAIT),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
        assert!(!trace.record_at_ns(Stage::Forwarded, 1, Tags::default()));
        assert!(
            writer
                .finish()
                .unwrap_err()
                .to_string()
                .contains("panicked")
        );
    }

    #[test]
    fn sink_flush_failure_never_returns_a_success_summary() {
        let bytes = MemorySink::default();
        let (trace, mut writer) = writer_for_test(FailingSink {
            bytes: bytes.clone(),
            fail_flush: true,
        });
        assert!(trace.record_at_ns(Stage::Forwarded, 1, Tags::default()));
        assert_eq!(
            writer.finish_timeout(TEST_WAIT).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert!(writer.summary.is_none());
        assert!(!trace.record_at_ns(Stage::Forwarded, 1, Tags::default()));
        assert!(bytes.rows().iter().all(|row| row["final"] != true));
    }

    struct GatedFlush {
        bytes: MemorySink,
        entered: Option<mpsc::Sender<()>>,
        release: Receiver<()>,
    }

    impl Write for GatedFlush {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            if let Some(entered) = self.entered.take() {
                entered.send(()).unwrap();
                self.release
                    .recv_timeout(TEST_WAIT)
                    .map_err(io::Error::other)?;
            }
            Ok(())
        }
    }

    #[test]
    fn blocked_sink_timeout_is_retryable_and_does_not_join() {
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (_trace, mut writer) = writer_for_test(GatedFlush {
            bytes: MemorySink::default(),
            entered: Some(entered_tx),
            release: release_rx,
        });
        writer.close();
        entered.recv_timeout(TEST_WAIT).unwrap();
        assert_eq!(writer.finish_timeout(Duration::ZERO).unwrap(), None);
        assert_eq!(
            writer.finish_timeout(Duration::from_millis(10)).unwrap(),
            None
        );
        assert!(writer.summary.is_none());
        release.send(()).unwrap();
        let summary = writer.finish_timeout(TEST_WAIT).unwrap().unwrap();
        assert_eq!(
            writer.finish_timeout(Duration::ZERO).unwrap(),
            Some(summary)
        );
        assert_eq!(writer.finish().unwrap(), summary);
    }

    #[test]
    fn dropping_writer_detaches_even_while_sink_is_blocked() {
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (_trace, writer) = writer_for_test(GatedFlush {
            bytes: MemorySink::default(),
            entered: Some(entered_tx),
            release: release_rx,
        });
        writer.close();
        entered.recv_timeout(TEST_WAIT).unwrap();
        drop(writer);
        release.send(()).unwrap();
    }

    #[test]
    fn unforwarded_or_stale_input_is_not_reported_as_protocol_delivery() {
        assert!(forwarded_keyboard_tags(7, None).is_none());
        assert!(forwarded_keyboard_tags(7, Some((6, 12, 100))).is_none());
        let tags = forwarded_keyboard_tags(7, Some((7, 12, 100))).unwrap();
        assert!(matches!(tags.correlation_id, Some(Tag::U64(7))));
        assert!(matches!(tags.source_event_serial, Some(Tag::U64(12))));
        assert!(matches!(tags.source_event_time, Some(Tag::U64(100))));
    }

    #[test]
    fn numeric_ids_are_nonzero_and_unique() {
        let first = next_id();
        let second = next_id();
        assert_ne!(first, 0);
        assert!(second > first);
    }

    #[test]
    fn clock_conversion_bounds() {
        assert_eq!(media_time_ns(0.0), Some(0));
        assert_eq!(media_time_ns(1.25), Some(1_250_000_000));
        assert!(media_time_ns(18_000_000_000.0).is_some());
        for seconds in [
            -1.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            u64::MAX as f64 / 1e9,
        ] {
            assert_eq!(media_time_ns(seconds), None);
        }
    }

    #[test]
    fn bounded_fast_path_and_disabled_trace() {
        let (trace, rx) = LatencyTrace::fast_for_test(1);
        assert!(trace.record_at_ns(Stage::Forwarded, 1, Tags::default()));
        assert!(!trace.record_at_ns(Stage::Forwarded, 2, Tags::default()));
        assert_eq!(trace.dropped_count(), 1);
        drop(rx);
        assert!(!trace.record_at_ns(Stage::Forwarded, 3, Tags::default()));
        assert_eq!(trace.dropped_count(), 2);
        assert!(!LatencyTrace::default().record(Stage::Forwarded, Tags::default()));
    }

    #[test]
    fn json_stages_and_tag_types_are_valid() {
        let (trace, rx) = LatencyTrace::fast_for_test(15);
        let stages = [
            Stage::HostNativeInput,
            Stage::Forwarded,
            Stage::SurfaceCommit,
            Stage::ComposeStarted,
            Stage::CPUSubmitted,
            Stage::MetalPresented,
            Stage::DrawableAcquireStarted,
            Stage::DrawableAcquireFinished,
            Stage::ComposeFinished,
            Stage::GPUStarted,
            Stage::GPUCompleted,
            Stage::GPUCompletionCallback,
            Stage::GPUCommandFailed,
            Stage::MetalPresentedCallback,
            Stage::FrameCallbackDone,
        ];
        let names = [
            "HostNativeInput",
            "Forwarded",
            "SurfaceCommit",
            "ComposeStarted",
            "CPUSubmitted",
            "MetalPresented",
            "DrawableAcquireStarted",
            "DrawableAcquireFinished",
            "ComposeFinished",
            "GPUStarted",
            "GPUCompleted",
            "GPUCompletionCallback",
            "GPUCommandFailed",
            "MetalPresentedCallback",
            "FrameCallbackDone",
        ];
        for (stage, name) in stages.into_iter().zip(names) {
            assert!(trace.record_at_ns(
                stage,
                42,
                Tags {
                    correlation_id: Some(Tag::String("input\"\n42")),
                    source_event_serial: Some(Tag::U64(u64::MAX)),
                    source_event_time: Some(Tag::String("native:1.25s")),
                    window_id: None,
                    probe_marker: None,
                    surface_id: Some(Tag::String("wl_surface@1[0]")),
                    commit_flags: Some(4),
                }
            ));
            let mut bytes = Vec::new();
            json_line(&mut bytes, &rx.recv().unwrap()).unwrap();
            let row: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(row["stage"], name);
            assert_eq!(row["clock"], HOST_CLOCK);
            assert_eq!(row["correlation_id"], "input\"\n42");
            assert_eq!(row["source_event_serial"].as_u64(), Some(u64::MAX));
            assert!(row["window_id"].is_null());
        }
    }

    #[test]
    fn frame_milestones_preserve_ids_window_and_queue_bound() {
        let (trace, rx) = LatencyTrace::fast_for_test(2);
        assert_eq!(
            trace.record_frame_at_ns(Stage::DrawableAcquireStarted, 42, &[11, 12, 13], 8),
            2
        );
        assert_eq!(trace.dropped_count(), 1);
        for id in [11, 12] {
            let row = serde_json::to_value(rx.try_recv().unwrap()).unwrap();
            assert_eq!(row["stage"], "DrawableAcquireStarted");
            assert_eq!(row["timestamp_ns"], 42);
            assert_eq!(row["correlation_id"], id);
            assert_eq!(row["window_id"], 8);
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn acquire_end_does_not_require_a_successful_submission() {
        let (trace, rx) = LatencyTrace::fast_for_test(2);
        for (stage, timestamp) in [
            (Stage::DrawableAcquireStarted, 10),
            (Stage::DrawableAcquireFinished, 30),
        ] {
            assert_eq!(trace.record_frame_at_ns(stage, timestamp, &[11], 8), 1);
        }
        let rows: Vec<_> = rx.try_iter().collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].timestamp_ns - rows[0].timestamp_ns, 20);
        assert!(rows.iter().all(|row| row.stage != Stage::CPUSubmitted));
    }

    #[test]
    fn gpu_api_times_and_callback_arrival_do_not_fake_presentation() {
        let (trace, rx) = LatencyTrace::fast_for_test(4);
        let tags = Tags {
            correlation_id: Some(Tag::U64(11)),
            window_id: Some(Tag::U64(8)),
            ..Default::default()
        };
        assert!(trace.record_at_ns(Stage::GPUCompletionCallback, 2_000_000_000, tags));
        assert!(trace.gpu_execution(1.0, 1.5, tags));
        assert!(trace.record_at_ns(Stage::MetalPresentedCallback, 3_000_000_000, tags));
        assert!(!trace.presented(0.0, tags));
        let rows: Vec<_> = rx.try_iter().collect();
        assert_eq!(
            rows.iter()
                .map(|row| (row.stage, row.timestamp_ns))
                .collect::<Vec<_>>(),
            vec![
                (Stage::GPUCompletionCallback, 2_000_000_000),
                (Stage::GPUStarted, 1_000_000_000),
                (Stage::GPUCompleted, 1_500_000_000),
                (Stage::MetalPresentedCallback, 3_000_000_000),
            ]
        );
        for row in rows {
            let row = serde_json::to_value(row).unwrap();
            assert_eq!(row["correlation_id"], 11);
            assert_eq!(row["window_id"], 8);
        }
        assert_eq!(
            trace.inner.as_ref().unwrap().counts.rejected.load(Relaxed),
            1
        );
    }

    #[test]
    fn unavailable_gpu_time_does_not_discard_the_other_api_timestamp() {
        let (trace, rx) = LatencyTrace::fast_for_test(2);
        assert!(!trace.gpu_execution(0.0, 1.0, Tags::default()));
        assert!(!trace.gpu_execution(1.0, 0.0, Tags::default()));
        let rows: Vec<_> = rx.try_iter().collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].stage, Stage::GPUCompleted);
        assert_eq!(rows[1].stage, Stage::GPUStarted);
        assert!(rows.iter().all(|row| row.timestamp_ns == 1_000_000_000));
        assert_eq!(
            trace.inner.as_ref().unwrap().counts.rejected.load(Relaxed),
            2
        );
    }

    #[test]
    fn gpu_api_times_reject_zero_invalid_reversed_and_callback_substitutes() {
        let (trace, rx) = LatencyTrace::fast_for_test(2);
        for (start, end) in [
            (0.0, 0.0),
            (2.0, 1.0),
            (-1.0, -1.0),
            (f64::NAN, f64::NAN),
            (f64::INFINITY, f64::INFINITY),
            (u64::MAX as f64 / 1e9, u64::MAX as f64 / 1e9),
        ] {
            assert!(!trace.gpu_execution(start, end, Tags::default()));
        }
        for stage in [Stage::GPUStarted, Stage::GPUCompleted] {
            assert!(!trace.record(stage, Tags::default()));
            assert!(!trace.record_at_ns(stage, 0, Tags::default()));
        }
        assert_eq!(
            trace.inner.as_ref().unwrap().counts.rejected.load(Relaxed),
            15
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn disabled_metal_diagnostics_do_not_sample_or_queue() {
        let trace = LatencyTrace::default();
        assert!(!trace.record(Stage::GPUCompletionCallback, Tags::default()));
        assert_eq!(
            trace.record_frame_at_ns(Stage::DrawableAcquireStarted, 1, &[11], 8),
            0
        );
        assert!(!trace.gpu_execution(f64::NAN, f64::NAN, Tags::default()));
        assert!(!trace.presented(1.0, Tags::default()));
        assert_eq!(trace.dropped_count(), 0);
    }

    #[test]
    fn rejects_fake_presentation_and_oversized_tags() {
        let (trace, rx) = LatencyTrace::fast_for_test(1);
        assert!(!trace.record(Stage::MetalPresented, Tags::default()));
        assert!(!trace.presented(0.0, Tags::default()));
        assert!(!trace.presented(f64::NAN, Tags::default()));
        let long = "x".repeat(MAX_TAG_BYTES + 1);
        assert!(!trace.record_at_ns(
            Stage::Forwarded,
            1,
            Tags {
                window_id: Some(Tag::String(&long)),
                ..Tags::default()
            }
        ));
        assert_eq!(
            trace.inner.as_ref().unwrap().counts.rejected.load(Relaxed),
            4
        );
        assert!(rx.try_recv().is_err());
        assert!(trace.presented(1.25, Tags::default()));
        assert_eq!(rx.recv().unwrap().timestamp_ns, 1_250_000_000);
    }
}

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
//! - Drop ALL trace clones (including callbacks), then call TraceWriter::finish()
//!   off the UI thread to drain/flush. No joining or file I/O occurs on trace drop.
//!   Abrupt process exit can lose buffered records; absence of final stats is
//!   explicitly reported by the analyzer. Initialization/finish errors are caller
//!   owned; async file errors are also logged by the worker.
//!
//! Times are host CoreAnimation nanoseconds, NOT Unix/Instant/guest time. Source
//! event time is an opaque tag, never a latency timestamp. Guest/calibration work
//! is deferred. Analyze with `python3 scripts/analyze-latency.py TRACE.jsonl`.

use serde::Serialize;
use std::fs::OpenOptions;
use std::io::{self, BufWriter, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const QUEUE_CAPACITY: usize = 1024;
pub const MAX_TAG_BYTES: usize = 256;
pub const HOST_CLOCK: &str = "host_ca";
const FLUSH_INTERVAL: Duration = Duration::from_millis(250);

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
    Forwarded,
    SurfaceCommit,
    ComposeStarted,
    CPUSubmitted,
    MetalPresented,
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
}

#[derive(Default)]
struct Counts {
    dropped: AtomicU64,
    rejected: AtomicU64,
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

/// Separately owned so dropping a trace on the main thread never waits for I/O.
pub struct TraceWriter(JoinHandle<io::Result<()>>);

impl TraceWriter {
    /// Wait for draining. All LatencyTrace clones must already have been dropped.
    pub fn finish(self) -> io::Result<()> {
        self.0
            .join()
            .map_err(|_| io::Error::other("latency writer panicked"))?
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
        let worker = thread::Builder::new()
            .name("latency-trace".into())
            .spawn(move || {
                let result = write_trace(path, rx, worker_counts);
                if let Err(error) = &result {
                    log::warn!("latency trace writer failed: {error}");
                }
                result
            })?;
        Ok((
            Self {
                inner: Some(Enabled { tx, counts }),
            },
            Some(TraceWriter(worker)),
        ))
    }

    pub fn enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Full/disconnected queue losses. Invalid times/tags are counted separately.
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
        if stage == Stage::MetalPresented {
            inner.counts.rejected.fetch_add(1, Relaxed);
            return false;
        }
        self.record_at_ns(stage, now_ns(), tags)
    }

    /// Caller-supplied timestamp MUST be in HOST_CLOCK, not source-event/guest time.
    pub fn record_at_ns(&self, stage: Stage, timestamp_ns: u64, tags: Tags<'_>) -> bool {
        let Some(inner) = &self.inner else {
            return false;
        };
        let values = [
            tags.correlation_id,
            tags.source_event_serial,
            tags.source_event_time,
            tags.window_id,
        ];
        if (stage == Stage::MetalPresented && timestamp_ns == 0)
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
        };
        if inner.tx.try_send(event).is_err() {
            inner.counts.dropped.fetch_add(1, Relaxed);
            return false;
        }
        true
    }

    /// Pass actual drawable.presentedTime seconds, not the callback's arrival time.
    /// https://developer.apple.com/documentation/metal/mtldrawable/presentedtime
    pub fn presented(&self, seconds: f64, tags: Tags<'_>) -> bool {
        let Some(inner) = &self.inner else {
            return false;
        };
        match media_time_ns(seconds).filter(|ns| *ns > 0) {
            Some(ns) => self.record_at_ns(Stage::MetalPresented, ns, tags),
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
) -> io::Result<()> {
    let session = format!("{}-{}-{}", std::process::id(), now_ns(), next_id());
    let mut out = BufWriter::new(OpenOptions::new().create(true).append(true).open(path)?);
    json_line(
        &mut out,
        &serde_json::json!({
            "schema": 1, "kind": "session", "session_id": session,
            "clock": HOST_CLOCK, "queue_capacity": QUEUE_CAPACITY,
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
        let final_stats = match rx.recv_timeout(FLUSH_INTERVAL) {
            Ok(event) => {
                json_line(
                    &mut out,
                    &Row {
                        schema: 1,
                        kind: "event",
                        session_id: &session,
                        event: &event,
                    },
                )?;
                false
            }
            Err(mpsc::RecvTimeoutError::Timeout) => false,
            Err(mpsc::RecvTimeoutError::Disconnected) => true,
        };
        if final_stats || last_flush.elapsed() >= FLUSH_INTERVAL {
            json_line(
                &mut out,
                &serde_json::json!({
                    "schema": 1, "kind": "stats", "session_id": session,
                    "dropped": counts.dropped.load(Relaxed),
                    "rejected": counts.rejected.load(Relaxed), "final": final_stats,
                }),
            )?;
            out.flush()?;
            last_flush = Instant::now();
        }
        if final_stats {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let (trace, rx) = LatencyTrace::fast_for_test(6);
        let stages = [
            Stage::HostNativeInput,
            Stage::Forwarded,
            Stage::SurfaceCommit,
            Stage::ComposeStarted,
            Stage::CPUSubmitted,
            Stage::MetalPresented,
        ];
        let names = [
            "HostNativeInput",
            "Forwarded",
            "SurfaceCommit",
            "ComposeStarted",
            "CPUSubmitted",
            "MetalPresented",
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

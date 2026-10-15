//! Standalone CPU/memory surrogate, not Metal or screen-presentation timing.
//! Compile with rustc -O --edition=2024; no Cargo dependencies or app startup.
use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::fs::OpenOptions;
use std::hint::black_box;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[allow(dead_code)]
#[path = "../src/texture_lifetime.rs"]
mod texture_lifetime;
use texture_lifetime::{FrameGate, FrameLifetime, TextureVersions};

unsafe extern "C" {
    fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> i32;
}

struct Mapping {
    ptr: *mut u8,
    len: usize,
}
impl Mapping {
    fn new(path: &Path, len: usize) -> Self {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap();
        file.set_len(len as u64).unwrap();
        // This private harness owns the file and never truncates the live mapping.
        let ptr = unsafe { mmap(std::ptr::null_mut(), len, 3, 1, file.as_raw_fd(), 0) };
        assert_ne!(ptr as isize, -1, "mmap failed");
        std::fs::remove_file(path).unwrap();
        let result = Self {
            ptr: ptr.cast(),
            len,
        };
        for i in 0..len {
            unsafe {
                *result.ptr.add(i) = (i.wrapping_mul(37) >> 3) as u8;
            }
        }
        result
    }
    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        assert_eq!(unsafe { munmap(self.ptr.cast(), self.len) }, 0);
    }
}

#[derive(Clone, Copy)]
struct Layout {
    width: usize,
    height: usize,
    stride: usize,
    offset: usize,
}
impl Layout {
    fn span(self) -> usize {
        (self.height - 1) * self.stride + self.width * 4
    }
    fn packed(self) -> usize {
        self.width * self.height * 4
    }
}

fn convert(source: &[u8], dest: &mut Vec<u8>, layout: Layout) {
    dest.resize(layout.packed(), 0);
    for y in 0..layout.height {
        let src = y * layout.stride;
        let dst = y * layout.width * 4;
        for (pixel, target) in source[src..src + layout.width * 4]
            .chunks_exact(4)
            .zip(dest[dst..dst + layout.width * 4].chunks_exact_mut(4))
        {
            target.copy_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
}

fn compact_swizzle(pixels: &mut Vec<u8>, layout: Layout) {
    for y in 0..layout.height {
        let src = y * layout.stride;
        let dst = y * layout.width * 4;
        pixels.copy_within(src..src + layout.width * 4, dst);
        for pixel in pixels[dst..dst + layout.width * 4].chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
    }
    pixels.truncate(layout.packed());
}

fn upload(source: &[u8], stride: usize, target: &mut [u8], layout: Layout, damage: bool) -> usize {
    let (w, h) = if damage {
        (64.min(layout.width), 64.min(layout.height))
    } else {
        (layout.width, layout.height)
    };
    for y in 0..h {
        let dst = y * layout.width * 4;
        target[dst..dst + w * 4].copy_from_slice(&source[y * stride..y * stride + w * 4]);
    }
    w * h * 4
}

type Storage = Rc<RefCell<Vec<u8>>>;
struct Arm {
    index: usize,
    snapshot: Vec<u8>,
    single: Storage,
    versions: TextureVersions<Storage>,
    gate: Arc<FrameGate>,
    pending: VecDeque<Arc<FrameLifetime>>,
    allocations: usize,
    samples: Vec<(u128, u128, u128, usize)>,
}
impl Arm {
    fn new(index: usize) -> Self {
        Self {
            index,
            snapshot: Vec::new(),
            single: Rc::new(RefCell::new(Vec::new())),
            versions: TextureVersions::new(),
            gate: Arc::new(FrameGate::default()),
            pending: VecDeque::new(),
            allocations: 0,
            samples: Vec::new(),
        }
    }
    fn step(&mut self, pool: &[u8], layout: Layout, swizzle: bool, damage: bool, record: bool) {
        let started = Instant::now();
        let raw = &pool[layout.offset..layout.offset + layout.span()];
        let source = if self.index == 0 {
            if swizzle {
                convert(raw, &mut self.snapshot, layout);
                &self.snapshot[..]
            } else {
                raw
            }
        } else {
            self.snapshot.clear();
            self.snapshot.reserve_exact(layout.span());
            self.snapshot.extend_from_slice(black_box(raw));
            if swizzle {
                compact_swizzle(&mut self.snapshot, layout);
            }
            &self.snapshot[..]
        };
        black_box(source);
        let prepared = Instant::now();
        let stride = if swizzle {
            layout.width * 4
        } else {
            layout.stride
        };
        let written;
        if self.index < 2 {
            let mut target = self.single.borrow_mut();
            let first = target.is_empty();
            if first {
                target.resize(layout.packed(), 0);
                self.allocations += 1;
            }
            written = upload(source, stride, &mut target, layout, damage && !first);
            black_box(&*target);
        } else {
            // Two still-outstanding readers force a third slot; completion is simulated.
            if self.pending.len() == 3 {
                self.pending.pop_front().unwrap().complete();
            }
            let frame = self.gate.begin().unwrap();
            let mut copied = 0;
            self.versions
                .update(|idle, is_current| {
                    let target = idle.cloned().unwrap_or_else(|| {
                        self.allocations += 1;
                        Rc::new(RefCell::new(vec![0; layout.packed()]))
                    });
                    copied = upload(
                        source,
                        stride,
                        &mut target.borrow_mut(),
                        layout,
                        damage && is_current,
                    );
                    black_box(&*target.borrow());
                    Some(target)
                })
                .unwrap();
            self.versions.mark_read(&frame);
            self.pending.push_back(frame);
            written = copied;
        }
        let finished = Instant::now();
        if record {
            self.samples.push((
                (prepared - started).as_nanos(),
                (finished - prepared).as_nanos(),
                (finished - started).as_nanos(),
                written,
            ));
        }
    }
}

fn quantiles(mut values: Vec<u128>) -> (u128, u128, u128) {
    values.sort_unstable();
    let rank = |p: usize| values[(values.len() * p).div_ceil(100) - 1];
    (rank(50), rank(95), rank(99))
}

fn bench(args: &[String]) {
    assert_eq!(
        args.len(),
        8,
        "bench WIDTH HEIGHT SAMPLES ROUNDS WARMUP MAP_PATH"
    );
    let number = |i: usize| args[i].parse::<usize>().unwrap();
    let (width, height, samples, rounds, warmup) =
        (number(2), number(3), number(4), number(5), number(6));
    assert!((64..=3840).contains(&width) && (64..=2160).contains(&height));
    assert!(
        (1..=2000).contains(&samples)
            && (1..=10).contains(&rounds)
            && samples * rounds <= 6000
            && warmup <= 200
    );
    let layout = Layout {
        width,
        height,
        stride: width * 4 + 64,
        offset: 128,
    };
    let mapping = Mapping::new(Path::new(&args[7]), layout.offset + layout.span());
    for (swizzle, damage, case) in [
        (false, false, "bgra_full"),
        (false, true, "bgra_damage64"),
        (true, true, "abgr_damage64"),
    ] {
        let mut arms: Vec<_> = (0..3).map(Arm::new).collect();
        for arm in &mut arms {
            for _ in 0..warmup.max(4) {
                arm.step(mapping.bytes(), layout, swizzle, damage, false);
            }
        }
        for round in 0..rounds {
            for iteration in 0..samples {
                // Deterministic rotating order, rather than always timing A first.
                for offset in 0..3 {
                    let arm = &mut arms[(round + iteration + offset) % 3];
                    arm.step(mapping.bytes(), layout, swizzle, damage, true);
                }
            }
        }
        for arm in arms {
            let name = [
                "direct_single_unsafe_baseline",
                "snapshot_single",
                "snapshot_ring3",
            ][arm.index];
            let (p50, p95, p99) = quantiles(arm.samples.iter().map(|s| s.2).collect());
            let (s50, s95, s99) = quantiles(arm.samples.iter().map(|s| s.0).collect());
            let (u50, u95, u99) = quantiles(arm.samples.iter().map(|s| s.1).collect());
            let count = arm.samples.len();
            let total_ns: u128 = arm.samples.iter().map(|s| s.2).sum();
            let prepare_ns: u128 = arm.samples.iter().map(|s| s.0).sum();
            let upload_ns: u128 = arm.samples.iter().map(|s| s.1).sum();
            let uploaded: usize = arm.samples.iter().map(|s| s.3).sum();
            let snapshot_bytes = if arm.index > 0 { layout.span() } else { 0 };
            let retained = arm.snapshot.capacity() + arm.allocations * layout.packed();
            println!(
                "{{\"kind\":\"cpu_copy\",\"case\":\"{case}\",\"arm\":\"{name}\",\"width\":{width},\"height\":{height},\"stride\":{},\"offset\":{},\"samples\":{count},\"rounds\":{rounds},\"warmup_per_arm\":{},\"source_mapping_bytes\":{},\"snapshot_bytes_per_frame\":{snapshot_bytes},\"upload_bytes_per_frame\":{},\"snapshot_capacity_bytes\":{},\"upload_slots\":{},\"retained_payload_capacity_bytes\":{retained},\"total_ns\":{total_ns},\"prepare_total_ns\":{prepare_ns},\"upload_total_ns\":{upload_ns},\"uploaded_bytes_total\":{uploaded},\"cpu_prepare_ns\":{{\"p50\":{s50},\"p95\":{s95},\"p99\":{s99}}},\"cpu_upload_surrogate_ns\":{{\"p50\":{u50},\"p95\":{u95},\"p99\":{u99}}},\"cpu_total_ns\":{{\"p50\":{p50},\"p95\":{p95},\"p99\":{p99}}}}}",
                layout.stride,
                layout.offset,
                warmup.max(4),
                mapping.len,
                uploaded / count,
                arm.snapshot.capacity(),
                arm.allocations
            );
        }
    }
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Default)]
struct Usage {
    uuid: [u8; 16],
    user_ns: u64,
    system_ns: u64,
    package_wakes: u64,
    interrupt_wakes: u64,
    pageins: u64,
    wired_bytes: u64,
    resident_bytes: u64,
    footprint_bytes: u64,
    start: u64,
    exit: u64,
}
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut c_void) -> i32;
}

#[cfg(target_os = "macos")]
fn usage() -> Option<Usage> {
    let mut data = Usage::default();
    (unsafe {
        proc_pid_rusage(
            std::process::id() as i32,
            0,
            (&mut data as *mut Usage).cast(),
        )
    } == 0)
        .then_some(data)
}

fn idle(args: &[String]) {
    use std::io::{BufRead, Write};
    assert_eq!(args.len(), 4, "idle event|poll SECONDS");
    let seconds = args[3].parse::<u64>().unwrap();
    assert!((1..=30).contains(&seconds) && matches!(args[2].as_str(), "event" | "poll"));
    println!("ready");
    std::io::stdout().flush().unwrap();
    let _ = std::io::stdin().lock().lines().next().unwrap().unwrap();
    #[cfg(target_os = "macos")]
    let before = usage();
    let start = Instant::now();
    let duration = Duration::from_secs(seconds);
    let mut waits = 0;
    if args[2] == "event" {
        let (_sender, receiver) = std::sync::mpsc::channel::<()>();
        assert!(receiver.recv_timeout(duration).is_err());
        waits = 1;
    } else {
        while start.elapsed() < duration {
            std::thread::sleep(Duration::from_millis(1));
            waits += 1;
        }
    }
    let wall_ns = start.elapsed().as_nanos();
    #[cfg(target_os = "macos")]
    let counters = match (before, usage()) {
        (Some(a), Some(b)) => format!(
            "{{\"cpu_ns\":{},\"interrupt_wakes\":{},\"package_idle_wakes\":{},\"resident_bytes_at_end\":{},\"physical_footprint_bytes_at_end\":{}}}",
            b.user_ns + b.system_ns - a.user_ns - a.system_ns,
            b.interrupt_wakes - a.interrupt_wakes,
            b.package_wakes - a.package_wakes,
            b.resident_bytes,
            b.footprint_bytes
        ),
        _ => "null".to_string(),
    };
    #[cfg(not(target_os = "macos"))]
    let counters = "null";
    println!(
        "{{\"kind\":\"synthetic_idle\",\"arm\":\"{}\",\"wall_ns\":{wall_ns},\"wait_returns\":{waits},\"macos_proc_pid_rusage_v0\":{counters}}}",
        args[2]
    );
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("bench") => bench(&args),
        Some("idle") => idle(&args),
        _ => panic!("expected bench or idle"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn padded_swizzle_matches_direct_and_keeps_source() {
        let layout = Layout {
            width: 2,
            height: 2,
            stride: 12,
            offset: 3,
        };
        let pool: Vec<_> = (0..40).collect();
        let source = &pool[layout.offset..layout.offset + layout.span()];
        let before = source.to_vec();
        let mut direct = Vec::new();
        convert(source, &mut direct, layout);
        let mut snapshot = source.to_vec();
        compact_swizzle(&mut snapshot, layout);
        assert_eq!(direct, snapshot);
        assert_eq!(source, before);
        assert_eq!(
            snapshot,
            [5, 4, 3, 6, 9, 8, 7, 10, 17, 16, 15, 18, 21, 20, 19, 22]
        );
    }
    #[test]
    fn ring_allocates_three_then_reuses_and_fully_uploads_damage() {
        let layout = Layout {
            width: 128,
            height: 128,
            stride: 576,
            offset: 3,
        };
        let pool = vec![17; layout.offset + layout.span()];
        let mut arm = Arm::new(2);
        for _ in 0..12 {
            arm.step(&pool, layout, false, true, true);
        }
        assert_eq!(arm.allocations, 3);
        assert!(arm.samples.iter().all(|s| s.3 == layout.packed()));
        assert!(
            arm.versions
                .current()
                .unwrap()
                .borrow()
                .iter()
                .all(|b| *b == 17)
        );
    }
    #[test]
    fn damaged_upload_preserves_other_pixels() {
        let layout = Layout {
            width: 128,
            height: 128,
            stride: 576,
            offset: 0,
        };
        let mut target = vec![9; layout.packed()];
        assert_eq!(
            upload(
                &vec![17; layout.span()],
                layout.stride,
                &mut target,
                layout,
                true
            ),
            16384
        );
        assert_eq!(target[0], 17);
        assert_eq!(target[64 * 4], 9);
        assert_eq!(target[64 * 128 * 4], 9);
        assert_eq!(quantiles((1..=100).collect()), (50, 95, 99));
    }
}

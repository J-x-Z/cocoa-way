use std::collections::HashMap;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{LazyLock, Mutex, Once};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::runtime_paths::{apple_container_data_root, build_child_path, find_command_path};

const RESOURCE_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const STORAGE_GROWTH_MINIMUM_BYTES: u64 = 8 * 1024 * 1024 * 1024;

static START_RESOURCE_SAMPLER: Once = Once::new();
static RESOURCE_SNAPSHOT: LazyLock<Mutex<ResourceSnapshot>> =
    LazyLock::new(|| Mutex::new(ResourceSnapshot::waiting()));
static CLIPBOARD_SNAPSHOT: LazyLock<Mutex<ClipboardSnapshot>> =
    LazyLock::new(|| Mutex::new(ClipboardSnapshot::default()));

#[derive(Clone, Debug, Serialize)]
pub struct ContainerResourceSnapshot {
    pub id: String,
    pub cpu_percent: Option<f64>,
    pub memory_usage_bytes: u64,
    pub memory_limit_bytes: u64,
    pub network_rx_bytes: u64,
    pub network_tx_bytes: u64,
    pub block_read_bytes: u64,
    pub block_write_bytes: u64,
    pub processes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ResourceSnapshot {
    pub available: bool,
    pub sampled_at_unix_ms: u128,
    pub error: Option<String>,
    pub containers: Vec<ContainerResourceSnapshot>,
    pub total_cpu_percent: Option<f64>,
    pub total_memory_usage_bytes: u64,
    pub total_memory_limit_bytes: u64,
    pub disk_available_bytes: Option<u64>,
}

impl ResourceSnapshot {
    fn waiting() -> Self {
        Self {
            available: false,
            sampled_at_unix_ms: 0,
            error: Some("Waiting for the first Apple Container resource sample".into()),
            containers: Vec::new(),
            total_cpu_percent: None,
            total_memory_usage_bytes: 0,
            total_memory_limit_bytes: 0,
            disk_available_bytes: available_disk_bytes(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ClipboardSnapshot {
    pub state: String,
    pub last_direction: Option<String>,
    pub last_mime: Option<String>,
    pub last_event_unix_ms: Option<u128>,
    pub host_to_guest_events: u64,
    pub guest_to_host_events: u64,
    pub guest_offers: u64,
    pub failures: u64,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct RawContainerStats {
    id: String,
    cpu_usage_usec: Option<u64>,
    cpu_utilization: Option<f64>,
    memory_usage_bytes: u64,
    memory_limit_bytes: u64,
    network_rx_bytes: u64,
    network_tx_bytes: u64,
    block_read_bytes: u64,
    block_write_bytes: u64,
    processes: u64,
}

impl RawContainerStats {
    fn cpu_percent(
        &self,
        previous_cpu_usec: Option<u64>,
        elapsed: Option<Duration>,
    ) -> Option<f64> {
        // Apple Container 1.7.1 reports the fraction of one core, not a percentage.
        if let Some(percent) = self
            .cpu_utilization
            .map(|fraction| fraction * 100.0)
            .filter(|percent| percent.is_finite() && *percent >= 0.0)
        {
            return Some(percent);
        }
        let elapsed = elapsed.filter(|elapsed| !elapsed.is_zero())?;
        let delta = self.cpu_usage_usec?.checked_sub(previous_cpu_usec?)?;
        Some(delta as f64 / (elapsed.as_secs_f64() * 1_000_000.0) * 100.0)
    }
}

pub fn start_resource_sampler() {
    START_RESOURCE_SAMPLER.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("cocoa-way-resources".into())
            .spawn(resource_sampler_loop);
    });
}

pub fn resource_snapshot() -> ResourceSnapshot {
    RESOURCE_SNAPSHOT.lock().unwrap().clone()
}

pub fn clipboard_snapshot() -> ClipboardSnapshot {
    CLIPBOARD_SNAPSHOT.lock().unwrap().clone()
}

pub fn record_clipboard_host_change(bytes: usize, mime: Option<&str>) {
    update_clipboard("Ready", "macOS -> Wayland", mime, |snapshot| {
        snapshot.host_to_guest_events = snapshot.host_to_guest_events.saturating_add(1);
        snapshot.last_error = None;
        if bytes == 0 {
            snapshot.state = "macOS clipboard cleared".into();
            snapshot.last_mime = None;
        }
    });
}

pub fn record_clipboard_guest_offer(mime: &str) {
    update_clipboard("Guest offer", "Wayland -> macOS", Some(mime), |snapshot| {
        snapshot.guest_offers = snapshot.guest_offers.saturating_add(1);
    });
}

pub fn record_clipboard_guest_install(bytes: usize) {
    update_clipboard("Ready", "Wayland -> macOS", None, |snapshot| {
        snapshot.guest_to_host_events = snapshot.guest_to_host_events.saturating_add(1);
        snapshot.last_error = None;
        if bytes == 0 {
            snapshot.state = "Wayland clipboard was empty".into();
        }
    });
}

pub fn record_clipboard_failure(error: impl Into<String>) {
    record_clipboard_failure_for("Wayland -> macOS", error);
}

pub fn record_clipboard_host_failure(error: impl Into<String>) {
    record_clipboard_failure_for("macOS -> Wayland", error);
}

fn record_clipboard_failure_for(direction: &str, error: impl Into<String>) {
    let error = error.into();
    update_clipboard("Error", direction, None, |snapshot| {
        snapshot.failures = snapshot.failures.saturating_add(1);
        snapshot.last_error = Some(crate::process_output::redaction::redact(&error));
    });
}

pub fn ensure_storage_growth_allowed() -> Result<u64, String> {
    let available = available_disk_bytes()
        .ok_or_else(|| "Unable to determine available disk space".to_string())?;
    if available < STORAGE_GROWTH_MINIMUM_BYTES {
        return Err(format!(
            "Only {:.1} GiB is free. Cocoa-Way requires at least 8 GiB before building, pulling, or loading an image.",
            bytes_to_gib(available)
        ));
    }
    Ok(available)
}

pub fn available_disk_bytes() -> Option<u64> {
    let path = apple_container_data_root();
    let fallback = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/"));
    filesystem_available_bytes(if path.exists() { &path } else { &fallback })
}

pub fn bytes_to_gib(bytes: u64) -> f64 {
    bytes as f64 / 1024.0 / 1024.0 / 1024.0
}

fn update_clipboard(
    state: &str,
    direction: &str,
    mime: Option<&str>,
    update: impl FnOnce(&mut ClipboardSnapshot),
) {
    let mut snapshot = CLIPBOARD_SNAPSHOT.lock().unwrap();
    snapshot.state = state.into();
    snapshot.last_direction = Some(direction.into());
    if let Some(mime) = mime {
        snapshot.last_mime = Some(mime.into());
    }
    snapshot.last_event_unix_ms = Some(unix_time_ms());
    update(&mut snapshot);
}

fn resource_sampler_loop() {
    let mut previous_cpu = HashMap::<String, u64>::new();
    let mut previous_sample = None;
    loop {
        let sample_started = Instant::now();
        let snapshot = sample_resources(&mut previous_cpu, &mut previous_sample, sample_started);
        if !snapshot.available {
            previous_cpu.clear();
            previous_sample = None;
        }
        *RESOURCE_SNAPSHOT.lock().unwrap() = snapshot;
        let elapsed = sample_started.elapsed();
        if elapsed < RESOURCE_SAMPLE_INTERVAL {
            std::thread::sleep(RESOURCE_SAMPLE_INTERVAL - elapsed);
        }
    }
}

fn sample_resources(
    previous_cpu: &mut HashMap<String, u64>,
    previous_sample: &mut Option<Instant>,
    sample_started: Instant,
) -> ResourceSnapshot {
    let child_path = build_child_path();
    let Some(container) = find_command_path("container", &child_path) else {
        return ResourceSnapshot {
            available: false,
            sampled_at_unix_ms: unix_time_ms(),
            error: Some("Apple Container command not found".into()),
            containers: Vec::new(),
            total_cpu_percent: None,
            total_memory_usage_bytes: 0,
            total_memory_limit_bytes: 0,
            disk_available_bytes: available_disk_bytes(),
        };
    };
    if let Ok(output) = run_command(
        &container,
        &["system", "status", "--format", "json"],
        &child_path,
        COMMAND_TIMEOUT,
    ) && let Ok(status) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
    {
        crate::runtime_paths::remember_apple_container_status(&status);
        if let Some(state) = status.get("status").and_then(serde_json::Value::as_str)
            && state != "running"
        {
            return resource_error(
                &format!("Apple Container service is {state}; start it from Runtimes."),
                available_disk_bytes(),
            );
        }
    }
    let disk_available_bytes = available_disk_bytes();
    let output = match run_command(
        &container,
        &["stats", "--no-stream", "--format", "json"],
        &child_path,
        crate::apple_runtime::STATS_TIMEOUT,
    ) {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            return resource_error(
                String::from_utf8_lossy(&output.stderr)
                    .lines()
                    .next()
                    .unwrap_or("container stats failed"),
                disk_available_bytes,
            );
        }
        Err(error) => return resource_error(&error, disk_available_bytes),
    };
    let raw = match parse_stats(&output.stdout) {
        Ok(raw) => raw,
        Err(error) => return resource_error(&error, disk_available_bytes),
    };
    let elapsed =
        previous_sample.and_then(|previous| sample_started.checked_duration_since(previous));
    let mut next_cpu = HashMap::new();
    let containers = raw
        .into_iter()
        .map(|stats| {
            let cpu_percent = stats.cpu_percent(previous_cpu.get(&stats.id).copied(), elapsed);
            if let Some(counter) = stats.cpu_usage_usec {
                next_cpu.insert(stats.id.clone(), counter);
            }
            ContainerResourceSnapshot {
                id: stats.id,
                cpu_percent,
                memory_usage_bytes: stats.memory_usage_bytes,
                memory_limit_bytes: stats.memory_limit_bytes,
                network_rx_bytes: stats.network_rx_bytes,
                network_tx_bytes: stats.network_tx_bytes,
                block_read_bytes: stats.block_read_bytes,
                block_write_bytes: stats.block_write_bytes,
                processes: stats.processes,
            }
        })
        .collect::<Vec<_>>();
    *previous_cpu = next_cpu;
    *previous_sample = Some(sample_started);
    let total_cpu_percent =
        sum_cpu_percent(containers.iter().map(|container| container.cpu_percent));
    ResourceSnapshot {
        available: true,
        sampled_at_unix_ms: unix_time_ms(),
        error: None,
        total_memory_usage_bytes: containers
            .iter()
            .map(|container| container.memory_usage_bytes)
            .sum(),
        total_memory_limit_bytes: containers
            .iter()
            .map(|container| container.memory_limit_bytes)
            .sum(),
        containers,
        total_cpu_percent,
        disk_available_bytes,
    }
}

fn sum_cpu_percent(values: impl IntoIterator<Item = Option<f64>>) -> Option<f64> {
    values
        .into_iter()
        .try_fold(0.0, |total, value| Some(total + value?))
}

fn parse_stats(output: &[u8]) -> Result<Vec<RawContainerStats>, String> {
    let value: serde_json::Value = serde_json::from_slice(output)
        .map_err(|error| format!("Invalid container stats JSON: {error}"))?;
    let rows = value
        .as_array()
        .ok_or_else(|| "container stats JSON was not an array".to_string())?;
    rows.iter()
        .map(|row| {
            Ok(RawContainerStats {
                id: string_field(row, "id")?,
                cpu_usage_usec: row.get("cpuUsageUsec").and_then(serde_json::Value::as_u64),
                cpu_utilization: row
                    .get("cpuUtilization")
                    .and_then(serde_json::Value::as_f64),
                memory_usage_bytes: u64_field(row, "memoryUsageBytes"),
                memory_limit_bytes: u64_field(row, "memoryLimitBytes"),
                network_rx_bytes: u64_field(row, "networkRxBytes"),
                network_tx_bytes: u64_field(row, "networkTxBytes"),
                block_read_bytes: u64_field(row, "blockReadBytes"),
                block_write_bytes: u64_field(row, "blockWriteBytes"),
                processes: u64_field(row, "numProcesses"),
            })
        })
        .collect()
}

fn string_field(value: &serde_json::Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("container stats row is missing {key}"))
}

fn u64_field(value: &serde_json::Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default()
}

fn resource_error(error: &str, disk_available_bytes: Option<u64>) -> ResourceSnapshot {
    ResourceSnapshot {
        available: false,
        sampled_at_unix_ms: unix_time_ms(),
        error: Some(crate::process_output::redaction::redact(error.trim())),
        containers: Vec::new(),
        total_cpu_percent: None,
        total_memory_usage_bytes: 0,
        total_memory_limit_bytes: 0,
        disk_available_bytes,
    }
}

fn run_command(
    path: &Path,
    args: &[&str],
    child_path: &str,
    timeout: Duration,
) -> Result<Output, String> {
    let mut command = Command::new(path);
    command.env("PATH", child_path).args(args);
    crate::runtime_commands::capture(command, timeout).map_err(|error| error.to_string())
}

pub(crate) fn filesystem_available_bytes(path: &Path) -> Option<u64> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    let stats = unsafe { stats.assume_init() };
    Some((stats.f_bavail as u64).saturating_mul(stats.f_bsize as u64))
}

fn unix_time_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{bytes_to_gib, parse_stats, resource_error, sum_cpu_percent};

    #[test]
    fn exported_resource_errors_are_sanitized_without_changing_measurements() {
        let snapshot = resource_error("  password=hidden  ", Some(123));
        assert_eq!(snapshot.error.as_deref(), Some("password=[REDACTED]"));
        assert_eq!(snapshot.disk_available_bytes, Some(123));
    }

    #[test]
    fn parses_apple_container_stats_json() {
        let stats = parse_stats(
            br#"[{"id":"demo","memoryUsageBytes":1024,"memoryLimitBytes":4096,"cpuUsageUsec":55,"networkRxBytes":7,"networkTxBytes":8,"blockReadBytes":9,"blockWriteBytes":10,"numProcesses":3}]"#,
        )
        .unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].id, "demo");
        assert_eq!(stats[0].cpu_usage_usec, Some(55));
        assert_eq!(stats[0].memory_limit_bytes, 4096);
        assert_eq!(stats[0].processes, 3);
    }

    #[test]
    fn uses_apple_container_1_7_1_cpu_utilization_on_the_first_sample() {
        let stats = parse_stats(
            br#"[{"id":"demo","cpuUsageUsec":55,"cpuUtilization":2.5,"memoryUsageBytes":1024,"memoryLimitBytes":4096,"numProcesses":3},{"id":"partial","cpuUtilization":null}]"#,
        ).unwrap();
        assert_eq!(stats[0].cpu_usage_usec, Some(55));
        assert_eq!(stats[0].cpu_percent(None, None), Some(250.0));
        assert_eq!(
            stats[0].cpu_percent(Some(50), Some(Duration::from_secs(1))),
            Some(250.0)
        );
        assert_eq!(stats[0].memory_usage_bytes, 1024);
        assert_eq!(stats[1].id, "partial");
        assert_eq!(
            stats[1].cpu_percent(Some(25), Some(Duration::from_secs(1))),
            None
        );
    }

    #[test]
    fn falls_back_to_comparable_legacy_cpu_counters_without_fabricating_idle() {
        let stats = parse_stats(br#"[{"id":"legacy","cpuUsageUsec":1750000},{"id":"bad-rate","cpuUsageUsec":1750000,"cpuUtilization":-1},{"id":"idle","cpuUtilization":0}]"#)
            .unwrap();
        let interval = Some(Duration::from_secs(2));
        assert_eq!(stats[0].cpu_percent(None, interval), None);
        assert_eq!(stats[0].cpu_percent(Some(2_000_000), interval), None);
        assert_eq!(
            stats[0].cpu_percent(Some(1_000_000), Some(Duration::ZERO)),
            None
        );
        assert_eq!(stats[0].cpu_percent(Some(1_000_000), interval), Some(37.5));
        assert_eq!(stats[0].cpu_percent(Some(1_750_000), interval), Some(0.0));
        assert_eq!(stats[1].cpu_percent(Some(1_000_000), interval), Some(37.5));
        assert_eq!(stats[2].cpu_percent(None, None), Some(0.0));
    }

    #[test]
    fn empty_cpu_total_is_positive_zero_and_missing_rates_remain_unknown() {
        assert_eq!(sum_cpu_percent([]).unwrap().to_bits(), 0.0f64.to_bits());
        assert_eq!(sum_cpu_percent([Some(150.0), Some(25.0)]), Some(175.0));
        assert_eq!(sum_cpu_percent([Some(150.0), None]), None);
    }

    #[test]
    fn converts_bytes_to_gibibytes() {
        assert!((bytes_to_gib(8 * 1024 * 1024 * 1024) - 8.0).abs() < f64::EPSILON);
    }
}

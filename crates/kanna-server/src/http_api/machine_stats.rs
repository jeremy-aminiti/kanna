use super::state::AppState;
use crate::db::Db;
use axum::extract::{Query, State};
use axum::Json;
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

mod native;
mod storage;

const REMOTE_STATS_TIMEOUT: Duration = Duration::from_secs(3);
const LOCAL_STATS_TIMEOUT: Duration = Duration::from_secs(2);
const SAMPLE_WINDOW: Duration = Duration::from_millis(500);
const MAX_SAMPLE_WINDOW: Duration = Duration::from_secs(5);
const CACHE_TTL: Duration = Duration::from_secs(2);
const MAX_PEERS: usize = 16;
const MAX_PROCESSES: usize = 8192;
const TOP_PER_RESOURCE: usize = 5;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MachineStatsQuery {
    #[serde(default)]
    local_only: bool,
    #[serde(default)]
    detailed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompactLoadAverages {
    five: Option<f64>,
    fifteen: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompactMachineStats {
    machine_id: String,
    load_averages: CompactLoadAverages,
    available_memory_bytes: Option<u64>,
    free_disk_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    errors: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompactMachineStatsResponse {
    machines: Vec<CompactMachineStats>,
    machine_errors: Vec<MachineError>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct LoadAverages {
    one: f64,
    five: f64,
    fifteen: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct MemoryStats {
    total_bytes: u64,
    used_bytes: u64,
    free_bytes: u64,
    available_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pressure: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    swap_total_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    swap_used_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compressed_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    collection_errors: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CpuStats {
    busy_percent: f64,
    idle_percent: f64,
    user_percent: f64,
    system_percent: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    io_wait_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    steal_percent: Option<f64>,
    sample_started_at: u64,
    sampled_at: u64,
    sample_window_ms: u64,
    source: String,
}

// Linux ordering: user, nice, system, idle, iowait, irq, softirq, steal.
// macOS maps its four states into this ordering; wait/steal are unavailable.
struct CpuTicks {
    values: [u64; 8],
    logical_cores: usize,
    source: &'static str,
    has_wait: bool,
}

fn cpu_delta(
    before: &CpuTicks,
    after: &CpuTicks,
    elapsed: Duration,
    started_at: u64,
    sampled_at: u64,
) -> Result<CpuStats, String> {
    if !(SAMPLE_WINDOW..=MAX_SAMPLE_WINDOW).contains(&elapsed) {
        return Err("CPU sample window outside 500–5000 ms; utilization unknown".into());
    }
    if before.logical_cores == 0
        || before.logical_cores != after.logical_cores
        || before.source != after.source
    {
        return Err("CPU topology or counter source changed/unavailable".into());
    }
    let mut delta = [0u64; 8];
    for (i, value) in delta.iter_mut().enumerate() {
        *value = after.values[i]
            .checked_sub(before.values[i])
            .ok_or("CPU counter reset/wrap; utilization unknown")?;
    }
    let total = delta
        .iter()
        .try_fold(0u64, |sum, value| sum.checked_add(*value))
        .ok_or("CPU counter overflow")?;
    if total == 0 {
        return Err("CPU counters did not advance; utilization unknown".into());
    }
    let percent = |value: u64| value as f64 * 100.0 / total as f64;
    let user = percent(delta[0] + delta[1]);
    let system = percent(delta[2] + delta[5] + delta[6]);
    Ok(CpuStats {
        busy_percent: percent(delta[0] + delta[1] + delta[2] + delta[5] + delta[6]),
        idle_percent: percent(delta[3]),
        user_percent: user,
        system_percent: system,
        io_wait_percent: after.has_wait.then(|| percent(delta[4])),
        steal_percent: after.has_wait.then(|| percent(delta[7])),
        sample_started_at: started_at,
        sampled_at,
        sample_window_ms: elapsed.as_millis() as u64,
        source: after.source.into(),
    })
}

struct ProcessCounter {
    pid: u32,
    parent_pid: u32,
    name: String,
    identity: (u64, u64),
    cpu_seconds: f64,
    resident_bytes: u64,
    at: Instant,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProcessStats {
    pid: u32,
    parent_pid: u32,
    name: String,
    // 100% means one logical CPU. Null means no valid pair, never idle.
    cpu_percent: Option<f64>,
    sample_window_ms: Option<u64>,
    resident_bytes: u64,
}

fn process_delta(before: Option<&ProcessCounter>, after: &ProcessCounter) -> ProcessStats {
    let window = before
        .filter(|b| b.identity == after.identity && b.pid == after.pid)
        .and_then(|b| {
            after
                .at
                .checked_duration_since(b.at)
                .map(|elapsed| (b, elapsed))
        })
        .filter(|(_, elapsed)| (SAMPLE_WINDOW..=MAX_SAMPLE_WINDOW).contains(elapsed));
    let usage = window.and_then(|(b, elapsed)| {
        let delta = after.cpu_seconds - b.cpu_seconds;
        (delta >= 0.0 && delta.is_finite()).then_some(delta * 100.0 / elapsed.as_secs_f64())
    });
    ProcessStats {
        pid: after.pid,
        parent_pid: after.parent_pid,
        name: bounded_text(&after.name, 128),
        cpu_percent: usage,
        sample_window_ms: usage.and(window.map(|(_, elapsed)| elapsed.as_millis() as u64)),
        resident_bytes: after.resident_bytes,
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProcessSummary {
    top_processes: Vec<ProcessStats>,
    observed_process_count: usize,
    sampled_process_count: usize,
    unavailable_process_count: usize,
    truncated: bool,
}

fn top_processes(mut rows: Vec<ProcessStats>) -> Vec<ProcessStats> {
    rows.sort_by(|a, b| {
        b.resident_bytes
            .cmp(&a.resident_bytes)
            .then(a.pid.cmp(&b.pid))
    });
    let mut selected: BTreeSet<u32> = rows.iter().take(TOP_PER_RESOURCE).map(|p| p.pid).collect();
    rows.sort_by(|a, b| {
        b.cpu_percent
            .unwrap_or(-1.0)
            .total_cmp(&a.cpu_percent.unwrap_or(-1.0))
            .then(a.pid.cmp(&b.pid))
    });
    selected.extend(
        rows.iter()
            .filter(|p| p.cpu_percent.is_some())
            .take(TOP_PER_RESOURCE)
            .map(|p| p.pid),
    );
    rows.into_iter()
        .filter(|p| selected.contains(&p.pid))
        .collect()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct MachineStats {
    machine_id: String,
    load_averages: LoadAverages,
    cpu_core_count: usize,
    memory: MemoryStats,
    heavy_process_count: usize,
    heavy_processes: BTreeMap<String, usize>,
    busy_task_count: u64,
    // New fields must remain absent for older peers; no healthy defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sampled_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    collection_window_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_age_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cpu: Option<CpuStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    physical_core_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    logical_core_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    processes: Option<ProcessSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    storage: Option<Vec<storage::StorageStats>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    collection_errors: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct MachineError {
    machine_id: Option<String>,
    error: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MachineStatsResponse {
    machines: Vec<MachineStats>,
    machine_errors: Vec<MachineError>,
}

pub(super) struct CachedStats {
    finished: Instant,
    result: Result<MachineStats, String>,
}

pub(super) struct CachedCompactStats {
    finished: Instant,
    result: CompactMachineStats,
}

// The worker owns the guard, even if every HTTP caller disconnects/times out.
// Thus there can be only one OS collector per server, including a stalled syscall.
// No permanent sampling task; a request starts a two-point sample on cache miss.
async fn local_stats(state: Arc<AppState>) -> Result<MachineStats, String> {
    cached_local_stats(Arc::clone(&state.machine_stats_cache), move || {
        gather_local_stats(&state)
    })
    .await
}

async fn cached_local_stats(
    cache: Arc<tokio::sync::Mutex<Option<CachedStats>>>,
    collect: impl FnOnce() -> Result<MachineStats, String> + Send + 'static,
) -> Result<MachineStats, String> {
    tokio::time::timeout(LOCAL_STATS_TIMEOUT, async {
        let mut cache = cache.lock_owned().await;
        if let Some(cached) = cache.as_ref().filter(|c| c.finished.elapsed() < CACHE_TTL) {
            let mut stats = cached.result.clone()?;
            stats.cache_age_ms = Some(cached.finished.elapsed().as_millis() as u64);
            return Ok(stats);
        }
        tokio::task::spawn_blocking(move || {
            let result = collect();
            *cache = Some(CachedStats {
                finished: Instant::now(),
                result: result.clone(),
            });
            result
        })
        .await
        .map_err(|e| format!("machine stats collector failed: {e}"))?
    })
    .await
    .map_err(|_| "local machine-stats collection timed out; capacity unknown".to_string())?
}

pub(super) async fn machine_stats(
    State(state): State<Arc<AppState>>,
    Query(query): Query<MachineStatsQuery>,
) -> Json<serde_json::Value> {
    if !query.detailed {
        return Json(
            serde_json::to_value(compact_machine_stats(state, query.local_only).await)
                .expect("compact machine stats serialize"),
        );
    }
    let peers = async {
        if query.local_only {
            return MachineStatsResponse {
                machines: vec![],
                machine_errors: vec![],
            };
        }
        gather_remote_detailed_stats(&state).await
    };
    let (local, mut response) = tokio::join!(local_stats(Arc::clone(&state)), peers);
    match local {
        Ok(stats) => response.machines.push(stats),
        Err(error) => response.machine_errors.push(MachineError {
            machine_id: Some(state.config.desktop_id.clone()),
            error,
        }),
    }
    response
        .machines
        .sort_by(|a, b| a.machine_id.cmp(&b.machine_id));
    Json(serde_json::to_value(response).expect("detailed machine stats serialize"))
}

async fn compact_machine_stats(
    state: Arc<AppState>,
    local_only: bool,
) -> CompactMachineStatsResponse {
    let peers = async {
        if local_only {
            CompactMachineStatsResponse {
                machines: vec![],
                machine_errors: vec![],
            }
        } else {
            gather_remote_compact_stats(&state).await
        }
    };
    let (local, mut response) = tokio::join!(cached_compact_local_stats(&state), peers);
    response.machines.push(local);
    response
        .machines
        .sort_by(|a, b| a.machine_id.cmp(&b.machine_id));
    response
}

async fn cached_compact_local_stats(state: &Arc<AppState>) -> CompactMachineStats {
    let cache = Arc::clone(&state.compact_machine_stats_cache);
    let state = Arc::clone(state);
    let fallback_id = state.config.desktop_id.clone();
    tokio::time::timeout(LOCAL_STATS_TIMEOUT, async {
        let mut cache = cache.lock_owned().await;
        if let Some(cached) = cache
            .as_ref()
            .filter(|cached| cached.finished.elapsed() < CACHE_TTL)
        {
            return cached.result.clone();
        }
        tokio::task::spawn_blocking(move || {
            let result = gather_local_compact_stats(&state);
            *cache = Some(CachedCompactStats {
                finished: Instant::now(),
                result: result.clone(),
            });
            result
        })
        .await
        .unwrap_or_else(|_| CompactMachineStats {
            machine_id: fallback_id.clone(),
            load_averages: CompactLoadAverages {
                five: None,
                fifteen: None,
            },
            available_memory_bytes: None,
            free_disk_bytes: None,
            errors: vec!["local stats collector stopped unexpectedly".into()],
        })
    })
    .await
    .unwrap_or_else(|_| CompactMachineStats {
        machine_id: fallback_id,
        load_averages: CompactLoadAverages {
            five: None,
            fifteen: None,
        },
        available_memory_bytes: None,
        free_disk_bytes: None,
        errors: vec!["local stats timed out; load, memory, and disk are unavailable".into()],
    })
}

fn gather_local_compact_stats(state: &AppState) -> CompactMachineStats {
    let load = System::load_average();
    let mut errors: Vec<String> = Vec::new();
    let load_averages = compact_load_averages(load.five, load.fifteen, &mut errors);
    let available_memory_bytes = match native::memory() {
        Ok(memory) => Some(memory.available_bytes),
        Err(_) => {
            errors.push("memory unavailable: local collector failed".into());
            None
        }
    };
    let free_disk_bytes = match Db::open(&state.config.db_path) {
        Ok(db) => {
            // Detailed storage errors contain a path-by-path inventory. Compact
            // callers only need to know whether a usable measurement exists.
            let mut storage_errors = Vec::new();
            let storage = storage::collect(&db, &mut storage_errors);
            storage::least_available_bytes(&storage)
        }
        Err(_) => {
            errors.push("disk unavailable: database could not be opened".into());
            None
        }
    };
    if free_disk_bytes.is_none()
        && !errors
            .iter()
            .any(|error| error.starts_with("disk unavailable"))
    {
        errors.push("disk unavailable: no backing volume could be measured".into());
    }
    compact_errors(&mut errors);
    CompactMachineStats {
        machine_id: state.config.desktop_id.clone(),
        load_averages,
        available_memory_bytes,
        free_disk_bytes,
        errors,
    }
}

async fn gather_remote_compact_stats(state: &Arc<AppState>) -> CompactMachineStatsResponse {
    let mut output = CompactMachineStatsResponse {
        machines: vec![],
        machine_errors: vec![],
    };
    let listing =
        tokio::time::timeout(REMOTE_STATS_TIMEOUT, state.list_active_relay_desktops()).await;
    let mut machine_ids = match listing {
        Ok(Ok(ids)) => ids,
        result => {
            let error = match result {
                Ok(Err(error)) => error,
                _ => "peer listing timed out; remote machine availability is unknown".into(),
            };
            output.machine_errors.push(MachineError {
                machine_id: None,
                error: bounded_text(&error, 160),
            });
            return output;
        }
    };
    machine_ids.sort();
    machine_ids.dedup();
    machine_ids.retain(|id| id != &state.config.desktop_id);
    if machine_ids.len() > MAX_PEERS {
        output.machine_errors.push(MachineError {
            machine_id: None,
            error: format!(
                "{} remote machines omitted (limit {MAX_PEERS})",
                machine_ids.len() - MAX_PEERS
            ),
        });
        machine_ids.truncate(MAX_PEERS);
    }
    let mut requests = FuturesUnordered::new();
    for machine_id in machine_ids {
        requests.push(async move {
            let result = tokio::time::timeout(
                REMOTE_STATS_TIMEOUT,
                state.invoke_relay_desktop(
                    machine_id.clone(),
                    "GET".into(),
                    "/v1/machine-stats?localOnly=true".into(),
                    serde_json::Value::Null,
                ),
            )
            .await;
            let result = match result {
                Err(_) => Err("unreachable: stats request timed out".into()),
                Ok(Err(error)) => Err(error),
                Ok(Ok(response)) if response.status == 200 => response
                    .body
                    .ok_or_else(|| "unavailable: stats response had no body".into())
                    .and_then(|body| decode_remote_compact(&machine_id, body)),
                Ok(Ok(response)) => Err(response
                    .error
                    .unwrap_or_else(|| format!("unavailable: HTTP {}", response.status))),
            };
            (machine_id, result)
        });
    }
    while let Some((machine_id, result)) = requests.next().await {
        match result {
            Ok(mut remote) => {
                output.machines.append(&mut remote.machines);
                output.machine_errors.append(&mut remote.machine_errors);
            }
            Err(error) => output.machine_errors.push(MachineError {
                machine_id: Some(bounded_text(&machine_id, 128)),
                error: bounded_text(&error, 160),
            }),
        }
    }
    output
}

fn decode_remote_compact(
    machine_id: &str,
    body: serde_json::Value,
) -> Result<CompactMachineStatsResponse, String> {
    let is_compact = body
        .get("machines")
        .and_then(serde_json::Value::as_array)
        .and_then(|machines| machines.first())
        .is_some_and(|machine| machine.get("availableMemoryBytes").is_some());
    let mut remote = if is_compact {
        serde_json::from_value::<CompactMachineStatsResponse>(body)
            .map_err(|error| format!("invalid compact stats response: {error}"))?
    } else {
        let detailed = decode_remote(machine_id, body)?;
        CompactMachineStatsResponse {
            machines: detailed
                .machines
                .into_iter()
                .map(compact_from_detailed)
                .collect(),
            machine_errors: detailed.machine_errors,
        }
    };
    if remote.machines.len() > 1
        || remote
            .machines
            .iter()
            .any(|machine| machine.machine_id != machine_id)
    {
        return Err("invalid localOnly machine-stats machine identity/count".into());
    }
    if remote.machines.is_empty() && remote.machine_errors.is_empty() {
        return Err("machine-stats response contained neither a snapshot nor an error".into());
    }
    for machine in &mut remote.machines {
        machine.machine_id = bounded_text(&machine.machine_id, 128);
        compact_errors(&mut machine.errors);
    }
    for error in &mut remote.machine_errors {
        error.machine_id = Some(bounded_text(machine_id, 128));
        error.error = bounded_text(&error.error, 160);
    }
    Ok(remote)
}

fn compact_from_detailed(machine: MachineStats) -> CompactMachineStats {
    // Detailed collection errors describe CPU sampling, process enumeration,
    // topology, and individual storage paths. None are compact-field errors by
    // themselves, so derive compact availability solely from the compact data.
    let mut errors = Vec::new();
    let load_averages = compact_load_averages(
        machine.load_averages.five,
        machine.load_averages.fifteen,
        &mut errors,
    );
    let free_disk_bytes = machine
        .storage
        .as_deref()
        .and_then(storage::least_available_bytes);
    if free_disk_bytes.is_none() {
        errors.push("disk unavailable: peer returned no storage measurement".into());
    }
    compact_errors(&mut errors);
    CompactMachineStats {
        machine_id: machine.machine_id,
        load_averages,
        available_memory_bytes: Some(machine.memory.available_bytes),
        free_disk_bytes,
        errors,
    }
}

fn compact_load_averages(five: f64, fifteen: f64, errors: &mut Vec<String>) -> CompactLoadAverages {
    let five = five.is_finite().then_some(five);
    let fifteen = fifteen.is_finite().then_some(fifteen);
    match (five.is_none(), fifteen.is_none()) {
        (true, true) => {
            errors.push("load unavailable: 5- and 15-minute averages could not be measured".into())
        }
        (true, false) => {
            errors.push("load unavailable: 5-minute average could not be measured".into())
        }
        (false, true) => {
            errors.push("load unavailable: 15-minute average could not be measured".into())
        }
        (false, false) => {}
    }
    CompactLoadAverages { five, fifteen }
}

fn compact_errors(errors: &mut Vec<String>) {
    errors.retain(|error| !error.trim().is_empty());
    if errors.len() > 4 {
        let omitted = errors.len() - 3;
        errors.truncate(3);
        errors.push(format!(
            "{omitted} additional errors omitted; use detailed=true"
        ));
    }
    for error in errors {
        *error = bounded_text(error, 160);
    }
}

async fn gather_remote_detailed_stats(state: &Arc<AppState>) -> MachineStatsResponse {
    let mut output = MachineStatsResponse {
        machines: vec![],
        machine_errors: vec![],
    };
    let listing =
        tokio::time::timeout(REMOTE_STATS_TIMEOUT, state.list_active_relay_desktops()).await;
    let mut machine_ids = match listing {
        Ok(Ok(ids)) => ids,
        result => {
            let error = match result {
                Ok(Err(e)) => e,
                _ => "machine-stats peer listing timed out".into(),
            };
            output.machine_errors.push(MachineError {
                machine_id: None,
                error: bounded_text(&error, 512),
            });
            return output;
        }
    };
    machine_ids.sort();
    machine_ids.dedup();
    machine_ids.retain(|id| id != &state.config.desktop_id);
    if machine_ids.len() > MAX_PEERS {
        output.machine_errors.push(MachineError {
            machine_id: None,
            error: format!(
                "peer limit {MAX_PEERS}; {} machines omitted, capacity unknown",
                machine_ids.len() - MAX_PEERS
            ),
        });
        machine_ids.truncate(MAX_PEERS);
    }
    let mut requests = FuturesUnordered::new();
    for machine_id in machine_ids {
        requests.push(async move {
            let result = tokio::time::timeout(
                REMOTE_STATS_TIMEOUT,
                state.invoke_relay_desktop(
                    machine_id.clone(),
                    "GET".into(),
                    "/v1/machine-stats?localOnly=true&detailed=true".into(),
                    serde_json::Value::Null,
                ),
            )
            .await;
            let result = match result {
                Err(_) => Err("machine-stats request timed out".into()),
                Ok(Err(error)) => Err(error),
                Ok(Ok(response)) if response.status == 200 => response
                    .body
                    .ok_or_else(|| "machine-stats response had no body".into())
                    .and_then(|body| decode_remote(&machine_id, body)),
                Ok(Ok(response)) => Err(response
                    .error
                    .unwrap_or_else(|| format!("HTTP {}", response.status))),
            };
            (machine_id, result)
        });
    }
    while let Some((machine_id, result)) = requests.next().await {
        match result {
            Ok(mut remote) => {
                output.machines.append(&mut remote.machines);
                output.machine_errors.append(&mut remote.machine_errors);
            }
            Err(error) => output.machine_errors.push(MachineError {
                machine_id: Some(bounded_text(&machine_id, 128)),
                error: bounded_text(&error, 512),
            }),
        }
    }
    output
}

fn decode_remote(
    machine_id: &str,
    body: serde_json::Value,
) -> Result<MachineStatsResponse, String> {
    // Check the envelope before decoding so a localOnly peer cannot fan out again
    // or smuggle unrelated machine identities into an account snapshot.
    let mut remote: MachineStatsResponse =
        serde_json::from_value(body).map_err(|e| format!("invalid machine-stats response: {e}"))?;
    if remote.machines.len() > 1 || remote.machines.iter().any(|m| m.machine_id != machine_id) {
        return Err("invalid localOnly machine-stats machine identity/count".into());
    }
    if remote.machines.is_empty() && remote.machine_errors.is_empty() {
        return Err("machine-stats response contained neither a snapshot nor an error".into());
    }
    if remote.machine_errors.len() > 16 {
        let omitted = remote.machine_errors.len() - 15;
        remote.machine_errors.truncate(15);
        remote.machine_errors.push(MachineError {
            machine_id: Some(machine_id.into()),
            error: format!("{omitted} additional peer collection errors omitted"),
        });
    }
    for error in &mut remote.machine_errors {
        error.machine_id = Some(bounded_text(machine_id, 128));
        error.error = bounded_text(&error.error, 512);
    }
    for machine in &mut remote.machines {
        machine.machine_id = bounded_text(&machine.machine_id, 128);
        if let Some(cpu) = &mut machine.cpu {
            cpu.source = bounded_text(&cpu.source, 128);
        }
        for text in [&mut machine.memory.source, &mut machine.memory.pressure]
            .into_iter()
            .flatten()
        {
            *text = bounded_text(text, 128);
        }
        if let Some(processes) = &mut machine.processes {
            if processes.top_processes.len() > TOP_PER_RESOURCE * 2 {
                processes.truncated = true;
                processes.top_processes.truncate(TOP_PER_RESOURCE * 2);
            }
            for process in &mut processes.top_processes {
                process.name = bounded_text(&process.name, 128);
            }
        }
        if let Some(storage) = &mut machine.storage {
            storage::bound_remote(storage);
        }
        bound_errors(&mut machine.collection_errors);
        bound_errors(&mut machine.memory.collection_errors);
        // Only the documented legacy categories may cross the aggregation boundary.
        machine
            .heavy_processes
            .retain(|name, _| BUILD_KINDS.contains(&name.as_str()));
    }
    Ok(remote)
}

fn bound_errors(errors: &mut Option<Vec<String>>) {
    if let Some(errors) = errors {
        truncate_errors(errors);
        for error in errors {
            *error = bounded_text(error, 512);
        }
    }
}
fn truncate_errors(errors: &mut Vec<String>) {
    if errors.len() > 32 {
        let omitted = errors.len() - 31;
        errors.truncate(31);
        errors.push(format!("{omitted} additional collection errors omitted"));
    }
}
fn bounded_text(text: &str, limit: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(limit)
        .collect()
}
fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
const BUILD_KINDS: [&str; 6] = [
    "bazel",
    "cargo",
    "nodeTestRunner",
    "rustc",
    "vitest",
    "xcodebuild",
];

fn gather_local_stats(state: &AppState) -> Result<MachineStats, String> {
    let collection_started = Instant::now();
    let db = Db::open(&state.config.db_path).map_err(|e| format!("machine stats database: {e}"))?;
    let busy_task_count = db
        .count_busy_tasks()
        .map_err(|e| format!("busy task count: {e}"))?;
    let mut errors = Vec::new();
    let mut system = System::new();
    let mut pids = native::process_ids(MAX_PROCESSES + 1)?;
    pids.sort();
    let observed_process_count = pids.len();
    pids.truncate(MAX_PROCESSES);
    let mut before_processes = BTreeMap::new();
    for pid in &pids {
        if let Ok(counter) = native::process_counter(*pid) {
            before_processes.insert(*pid, counter);
        }
    }
    // Only Node-family runners need argv inspection. Keep it private and avoid
    // a second machine-wide sysinfo scan (including Linux threads).
    let runners: Vec<_> = before_processes
        .values()
        .filter(|p| matches!(p.name.as_str(), "node" | "nodejs" | "bun"))
        .map(|p| sysinfo::Pid::from_u32(p.pid))
        .collect();
    if !runners.is_empty() {
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&runners),
            true,
            ProcessRefreshKind::nothing().with_cmd(UpdateKind::OnlyIfNotSet),
        );
    }
    let missing_runner_commands = runners
        .iter()
        .filter(|pid| system.process(**pid).is_none_or(|p| p.cmd().is_empty()))
        .count();
    if missing_runner_commands > 0 {
        errors.push(format!("{missing_runner_commands} Node-family command lines unavailable; recognized build/test counts are partial"));
    }
    let mut heavy_processes: BTreeMap<String, usize> =
        BUILD_KINDS.iter().map(|name| ((*name).into(), 0)).collect();
    for process in before_processes.values() {
        let kind = system
            .process(sysinfo::Pid::from_u32(process.pid))
            .and_then(heavy_process_kind)
            .or_else(|| heavy_process_kind_from_command(&process.name.to_ascii_lowercase(), &[]));
        if let Some(count) = kind.and_then(|kind| heavy_processes.get_mut(kind)) {
            *count += 1;
        }
    }
    let before_cpu = native::cpu_ticks();
    let started_at = unix_ms();
    let start = Instant::now();
    std::thread::sleep(SAMPLE_WINDOW);
    let after_cpu = native::cpu_ticks();
    let elapsed = start.elapsed();
    let sampled_at = unix_ms();
    let logical_core_count = after_cpu
        .as_ref()
        .ok()
        .map(|c| c.logical_cores)
        .filter(|c| *c > 0);
    let cpu = match before_cpu.and_then(|before| {
        after_cpu.and_then(|after| cpu_delta(&before, &after, elapsed, started_at, sampled_at))
    }) {
        Ok(cpu) => Some(cpu),
        Err(error) => {
            errors.push(error);
            None
        }
    };
    let mut rows = Vec::new();
    let mut unavailable = 0;
    for pid in &pids {
        match native::process_counter(*pid) {
            Ok(after) => {
                let row = process_delta(before_processes.get(pid), &after);
                if row.cpu_percent.is_none() {
                    unavailable += 1;
                }
                rows.push(row);
            }
            Err(_) => unavailable += 1,
        }
    }
    let sampled_process_count = rows.iter().filter(|p| p.cpu_percent.is_some()).count();
    if unavailable > 0 {
        errors.push(format!("{unavailable} processes lack valid counters: permission denied, exited, PID reused, or sample window invalid; process coverage is partial"));
    }
    if observed_process_count == 0 {
        errors.push(
            "process enumeration returned no processes; counts and top processes are unavailable"
                .into(),
        );
    }
    if observed_process_count > MAX_PROCESSES {
        errors.push(format!("process inspection limited to {MAX_PROCESSES} PIDs; more exist, recognized counts and top lists are partial"));
    }
    let memory = native::memory()?;
    let physical_core_count = system.physical_core_count().filter(|c| *c > 0);
    if physical_core_count.is_none() {
        errors.push("physical core count unavailable".into());
    }
    let storage = storage::collect(&db, &mut errors);
    truncate_errors(&mut errors);
    let load = System::load_average();
    Ok(MachineStats {
        machine_id: state.config.desktop_id.clone(),
        load_averages: LoadAverages {
            one: load.one,
            five: load.five,
            fifteen: load.fifteen,
        },
        // Legacy field keeps physical-first semantics and its original fallback.
        cpu_core_count: physical_core_count.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1)
        }),
        memory,
        heavy_process_count: heavy_processes.values().sum(),
        heavy_processes,
        busy_task_count,
        sampled_at: Some(unix_ms()),
        collection_window_ms: Some(collection_started.elapsed().as_millis() as u64),
        cache_age_ms: Some(0),
        cpu,
        physical_core_count,
        logical_core_count,
        processes: (observed_process_count > 0).then(|| ProcessSummary {
            top_processes: top_processes(rows),
            observed_process_count,
            sampled_process_count,
            unavailable_process_count: unavailable,
            truncated: observed_process_count > MAX_PROCESSES,
        }),
        storage: Some(storage),
        collection_errors: Some(errors),
    })
}
fn heavy_process_kind(process: &sysinfo::Process) -> Option<&'static str> {
    let name = process.name().to_string_lossy().to_ascii_lowercase();
    let arguments = process
        .cmd()
        .iter()
        .map(|part| part.to_string_lossy().to_ascii_lowercase())
        .collect::<Vec<_>>();
    let arguments = arguments.iter().map(String::as_str).collect::<Vec<_>>();
    heavy_process_kind_from_command(&name, &arguments)
}

fn heavy_process_kind_from_command(name: &str, arguments: &[&str]) -> Option<&'static str> {
    let node = matches!(name, "node" | "nodejs" | "bun");
    if name == "rustc" {
        Some("rustc")
    } else if name == "cargo" {
        Some("cargo")
    } else if name == "bazel" || name == "bazelisk" {
        Some("bazel")
    } else if name == "vitest"
        || (node
            && arguments
                .iter()
                .any(|argument| tool_argument(argument, "vitest")))
    {
        Some("vitest")
    } else if name == "xcodebuild" {
        Some("xcodebuild")
    } else if matches!(name, "jest" | "mocha" | "ava" | "tap")
        || (node && is_node_test_runner(arguments))
    {
        Some("nodeTestRunner")
    } else {
        None
    }
}

fn is_node_test_runner(arguments: &[&str]) -> bool {
    arguments.iter().any(|argument| {
        *argument == "--test"
            || ["jest", "mocha", "ava", "tap"]
                .iter()
                .any(|tool| tool_argument(argument, tool))
    }) || arguments.iter().enumerate().any(|(index, argument)| {
        tool_argument(argument, "playwright") && arguments[index + 1..].contains(&"test")
    })
}

fn tool_argument(argument: &str, tool: &str) -> bool {
    argument.split('/').any(|part| {
        part == tool
            || part
                .strip_prefix(tool)
                .is_some_and(|suffix| suffix.starts_with('.'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_node_test_runner_command_shapes() {
        let cases = [
            (
                "jest",
                vec!["node", "node_modules/jest/bin/jest.js"],
                Some("nodeTestRunner"),
            ),
            (
                "mocha",
                vec!["node", "node_modules/mocha/bin/mocha.js"],
                Some("nodeTestRunner"),
            ),
            (
                "ava",
                vec!["node", "node_modules/ava/entrypoints/cli.mjs"],
                Some("nodeTestRunner"),
            ),
            (
                "tap",
                vec!["node", "node_modules/tap/bin/run.js"],
                Some("nodeTestRunner"),
            ),
            (
                "node test",
                vec!["node", "--test", "test/unit.js"],
                Some("nodeTestRunner"),
            ),
            (
                "playwright",
                vec!["node", "node_modules/playwright/cli.js", "test"],
                Some("nodeTestRunner"),
            ),
            (
                "vitest",
                vec!["node", "node_modules/vitest/vitest.mjs", "run"],
                Some("vitest"),
            ),
            ("application", vec!["node", "server.js"], None),
            (
                "playwright browser",
                vec!["node", "node_modules/playwright/cli.js", "install"],
                None,
            ),
        ];

        for name in ["nodejs", "bun"] {
            assert_eq!(
                heavy_process_kind_from_command(name, &["node_modules/vitest/vitest.mjs", "run"]),
                Some("vitest")
            );
        }
        for name in ["jest", "mocha", "ava", "tap"] {
            assert_eq!(
                heavy_process_kind_from_command(name, &[]),
                Some("nodeTestRunner")
            );
        }
        assert_eq!(
            heavy_process_kind_from_command("bazelisk", &[]),
            Some("bazel")
        );
        assert_eq!(
            heavy_process_kind_from_command("node", &["server.js", "--title=vitest"]),
            None
        );

        for (label, arguments, expected) in cases {
            assert_eq!(
                heavy_process_kind_from_command("node", &arguments),
                expected,
                "{label}"
            );
        }
    }

    fn ticks(values: [u64; 8]) -> CpuTicks {
        CpuTicks {
            values,
            logical_cores: 10,
            source: "fixture",
            has_wait: true,
        }
    }

    #[test]
    fn cpu_delta_is_machine_normalized_and_keeps_wait_and_steal_distinct() {
        let before = ticks([100; 8]);
        let after = ticks([120, 110, 140, 105, 107, 106, 108, 104]);
        let cpu = cpu_delta(&before, &after, SAMPLE_WINDOW, 1000, 1500).unwrap();
        assert_eq!(cpu.user_percent, 30.0);
        assert_eq!(cpu.system_percent, 54.0);
        assert_eq!(cpu.busy_percent, 84.0);
        assert_eq!(cpu.idle_percent, 5.0);
        assert_eq!(cpu.io_wait_percent, Some(7.0));
        assert_eq!(cpu.steal_percent, Some(4.0));
        assert_eq!(cpu.sample_window_ms, 500);
    }

    #[test]
    fn invalid_windows_and_counters_are_unknown_never_idle() {
        let before = ticks([100; 8]);
        let after = ticks([200; 8]);
        for elapsed in [
            Duration::ZERO,
            Duration::from_millis(499),
            Duration::from_secs(6),
        ] {
            assert!(cpu_delta(&before, &after, elapsed, 0, 0).is_err());
        }
        assert!(cpu_delta(&before, &before, SAMPLE_WINDOW, 0, 0).is_err());
        assert!(cpu_delta(&after, &before, SAMPLE_WINDOW, 0, 0).is_err());
        let mut topology = ticks([200; 8]);
        topology.logical_cores = 20;
        assert!(cpu_delta(&before, &topology, SAMPLE_WINDOW, 0, 0).is_err());
        let mut wait_reset = ticks([200; 8]);
        wait_reset.values[4] = 99; // Linux iowait can decrease. Do not clamp it to healthy zero.
        assert!(cpu_delta(&before, &wait_reset, SAMPLE_WINDOW, 0, 0).is_err());
    }

    #[test]
    fn non_build_consumers_can_saturate_cpu() {
        let cpu = cpu_delta(
            &ticks([0; 8]),
            &ticks([28, 0, 72, 0, 0, 0, 0, 0]),
            SAMPLE_WINDOW,
            0,
            500,
        )
        .unwrap();
        assert_eq!(cpu.busy_percent, 100.0);
        assert_eq!(cpu.idle_percent, 0.0);
        let at = Instant::now();
        let before = ProcessCounter {
            pid: 42,
            parent_pid: 1,
            name: "QEMULauncher".into(),
            identity: (1, 0),
            cpu_seconds: 10.0,
            resident_bytes: 1 << 30,
            at,
        };
        let after = ProcessCounter {
            name: before.name.clone(),
            cpu_seconds: 12.0,
            at: at + SAMPLE_WINDOW,
            ..before
        };
        let row = process_delta(Some(&before), &after);
        assert_eq!(row.cpu_percent, Some(400.0)); // four CPUs, not 400% of the machine
        assert_eq!(row.parent_pid, 1);
        assert_eq!(heavy_process_kind_from_command(&row.name, &[]), None);
        assert_eq!(top_processes(vec![row])[0].name, "QEMULauncher");
    }

    #[test]
    fn process_missing_baseline_or_reused_pid_is_unknown() {
        let before = ProcessCounter {
            pid: 42,
            parent_pid: 1,
            name: "worker".into(),
            identity: (1, 0),
            cpu_seconds: 10.0,
            resident_bytes: 4096,
            at: Instant::now(),
        };
        let mut after = ProcessCounter {
            name: before.name.clone(),
            at: before.at + SAMPLE_WINDOW,
            ..before
        };
        assert_eq!(process_delta(None, &after).cpu_percent, None);
        assert_eq!(process_delta(Some(&before), &after).cpu_percent, Some(0.0));
        after.identity = (2, 0);
        assert_eq!(process_delta(Some(&before), &after).cpu_percent, None);
        after.identity = before.identity;
        after.cpu_seconds = 9.0;
        assert_eq!(process_delta(Some(&before), &after).cpu_percent, None);
    }

    #[test]
    fn top_list_includes_memory_hogs_and_is_bounded() {
        let rows = (0..30)
            .map(|pid| ProcessStats {
                pid,
                parent_pid: 1,
                name: format!("consumer{pid}"),
                cpu_percent: Some(pid as f64),
                sample_window_ms: Some(500),
                resident_bytes: u64::from(30 - pid),
            })
            .collect();
        let top = top_processes(rows);
        assert_eq!(top.len(), 10);
        assert!(top.iter().any(|p| p.pid == 0));
        assert!(top.iter().any(|p| p.pid == 29));
    }

    fn old_remote() -> serde_json::Value {
        serde_json::json!({"machines": [{
            "machineId": "old", "loadAverages": {"one": 12, "five": 12, "fifteen": 12},
            "cpuCoreCount": 10,
            "memory": {"totalBytes": 100, "usedBytes": 90, "freeBytes": 1, "availableBytes": 10},
            "heavyProcessCount": 0, "heavyProcesses": {}, "busyTaskCount": 0
        }], "machineErrors": []})
    }

    #[test]
    fn old_remote_payload_keeps_new_metrics_unknown() {
        let result = serde_json::to_value(decode_remote("old", old_remote()).unwrap()).unwrap();
        let row = &result["machines"][0];
        for field in [
            "cpu",
            "processes",
            "sampledAt",
            "cacheAgeMs",
            "physicalCoreCount",
            "logicalCoreCount",
            "storage",
            "collectionErrors",
        ] {
            assert!(row.get(field).is_none(), "{field} fabricated for old peer");
        }
        assert!(row["memory"].get("swapUsedBytes").is_none());
    }

    #[test]
    fn remote_collection_errors_survive_and_bad_envelopes_fail() {
        let mut body = old_remote();
        body["machineErrors"] = serde_json::json!([{"machineId": "old", "error": "CPU denied"}]);
        assert_eq!(
            decode_remote("old", body).unwrap().machine_errors[0].error,
            "CPU denied"
        );
        assert!(decode_remote("someone-else", old_remote()).is_err());
        assert!(decode_remote(
            "old",
            serde_json::json!({"machines": [], "machineErrors": []})
        )
        .is_err());
        let failure = decode_remote("old", serde_json::json!({"machines": [], "machineErrors": [{"machineId": "old", "error": "collection failed"}]})).unwrap();
        assert!(failure.machines.is_empty());
        assert_eq!(failure.machine_errors[0].error, "collection failed");
    }

    #[tokio::test]
    async fn concurrent_callers_share_collection_and_failures() {
        let cache = Arc::new(tokio::sync::Mutex::new(None));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let collect = || {
            let calls = Arc::clone(&calls);
            move || {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err("counter unavailable".into())
            }
        };
        let (a, b) = tokio::join!(
            cached_local_stats(Arc::clone(&cache), collect()),
            cached_local_stats(cache, collect())
        );
        assert_eq!(a.unwrap_err(), "counter unavailable");
        assert_eq!(b.unwrap_err(), "counter unavailable");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancelled_request_keeps_worker_ownership_until_collection_finishes() {
        let cache = Arc::new(tokio::sync::Mutex::new(None));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let request_cache = Arc::clone(&cache);
        let request = tokio::spawn(async move {
            cached_local_stats(request_cache, move || {
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(30)).unwrap();
                Err("finished after cancellation".into())
            })
            .await
        });
        started_rx.await.unwrap();
        request.abort();
        let _ = request.await;
        assert!(
            cache.try_lock().is_err(),
            "cancelled HTTP request released live collector"
        );
        release_tx.send(()).unwrap();
        let result = cached_local_stats(cache, || panic!("started a duplicate collector")).await;
        assert_eq!(result.unwrap_err(), "finished after cancellation");
    }
}

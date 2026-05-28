use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{after, bounded, select, Receiver, Sender};
use serde::Deserialize;
use serde_json::{json, Value};

use super::cache::{InspectCache, Tier2ContributionUpdates};
use super::dispatch::{default_worker, start_dispatch_loop, InspectWorker};
use super::freshness::{verify_contribution_file, ContributionFreshness};
use super::job::{
    normalize_path, CallgraphSnapshot, FileContribution, InspectCategory, InspectJob,
    InspectResult, InspectScanSuccess, InspectSnapshot, JobKey, JobOutcome, JobScope,
};

const DEFAULT_SOFT_DEADLINE: Duration = Duration::from_secs(1);

type WaiterTx = Sender<JobOutcome>;

#[derive(Clone)]
struct Waiter {
    tx: WaiterTx,
}

pub struct InspectManager {
    request_tx: Sender<InspectJob>,
    result_rx: Receiver<InspectResult>,
    #[allow(dead_code)]
    pool: Arc<rayon::ThreadPool>,
    in_flight: Mutex<HashMap<JobKey, Vec<Waiter>>>,
    caches: Mutex<HashMap<PathBuf, Arc<InspectCache>>>,
    soft_deadline: Duration,
    next_job_id: AtomicU64,
}

impl InspectManager {
    pub fn new() -> Self {
        Self::with_worker(default_worker(), DEFAULT_SOFT_DEADLINE)
    }

    #[doc(hidden)]
    pub fn with_worker(worker: InspectWorker, soft_deadline: Duration) -> Self {
        let handles = start_dispatch_loop(worker);
        Self {
            request_tx: handles.request_tx,
            result_rx: handles.result_rx,
            pool: handles.pool,
            in_flight: Mutex::new(HashMap::new()),
            caches: Mutex::new(HashMap::new()),
            soft_deadline,
            next_job_id: AtomicU64::new(1),
        }
    }

    pub fn submit_category(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
    ) -> JobOutcome {
        self.submit_category_with_callgraph(snapshot, category, caller_scope, None)
    }

    pub fn submit_category_with_callgraph(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> JobOutcome {
        if !category.is_active() {
            return JobOutcome::Failed {
                message: format!("inspect category '{category}' is disabled in v0.33"),
            };
        }

        let cache = match self.cache_for_snapshot(&snapshot) {
            Ok(cache) => cache,
            Err(message) => return JobOutcome::Failed { message },
        };
        let key = JobKey::for_category_scope(category, &caller_scope);
        let (waiter_tx, waiter_rx) = bounded(1);

        match self.enqueue_with_waiter(
            snapshot,
            category,
            caller_scope.clone(),
            key.clone(),
            waiter_tx,
            callgraph_snapshot,
        ) {
            Ok(()) => self.wait_for_outcome(key, caller_scope, cache, waiter_rx),
            Err(message) => JobOutcome::Failed { message },
        }
    }

    pub fn submit_background(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
    ) -> Result<JobKey, String> {
        self.submit_background_with_callgraph(snapshot, category, caller_scope, None)
    }

    pub fn submit_background_with_callgraph(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> Result<JobKey, String> {
        if !category.is_active() {
            return Err(format!(
                "inspect category '{category}' is disabled in v0.33"
            ));
        }
        let key = JobKey::for_category_scope(category, &caller_scope);
        self.enqueue_without_waiter(
            snapshot,
            category,
            caller_scope,
            key.clone(),
            callgraph_snapshot,
        )?;
        Ok(key)
    }

    pub fn drain_completions(&self) -> usize {
        let mut drained = 0usize;
        while let Ok(result) = self.result_rx.try_recv() {
            self.route_completion(result);
            drained += 1;
        }
        drained
    }

    pub fn cache_for_snapshot(
        &self,
        snapshot: &InspectSnapshot,
    ) -> Result<Arc<InspectCache>, String> {
        self.cache_for_paths(snapshot.inspect_dir.clone(), snapshot.project_root.clone())
    }

    pub fn cache_for_paths(
        &self,
        inspect_dir: PathBuf,
        project_root: PathBuf,
    ) -> Result<Arc<InspectCache>, String> {
        let project_key = crate::search_index::project_cache_key(&project_root);
        let sqlite_path = inspect_dir.join(format!("{project_key}.sqlite"));
        let mut caches = self
            .caches
            .lock()
            .map_err(|_| "inspect manager cache map lock poisoned".to_string())?;
        if let Some(cache) = caches.get(&sqlite_path) {
            return Ok(Arc::clone(cache));
        }
        let cache = Arc::new(
            InspectCache::open(inspect_dir, project_root)
                .map_err(|error| format!("failed to open inspect cache: {error}"))?,
        );
        caches.insert(sqlite_path, Arc::clone(&cache));
        Ok(cache)
    }

    pub fn tier2_run_with_reuse(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> JobOutcome {
        let result = self.tier2_run_with_reuse_result(snapshot, category, callgraph_snapshot);
        let outcome = match result.outcome {
            Ok(success) => JobOutcome::Fresh {
                payload: success.aggregate,
            },
            Err(message) => JobOutcome::Failed { message },
        };
        filter_outcome_for_scope(outcome, &caller_scope)
    }

    /// Read-only Tier 2 aggregate lookup for `aft_inspect`. Does NOT run any
    /// scanner or freshness pass — returns the latest cached aggregate if
    /// present, otherwise `Pending`. This is the non-blocking variant intended
    /// for the synchronous `inspect` command path; Tier 2 scans run via
    /// `tier2_run_with_reuse` triggered by `aft_inspect_tier2_run` on
    /// `session.idle`.
    pub fn tier2_read_cached(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
    ) -> JobOutcome {
        if !category.is_active() {
            return JobOutcome::Failed {
                message: format!("inspect category '{category}' is disabled in v0.33"),
            };
        }
        if !category.is_tier2() {
            return JobOutcome::Failed {
                message: format!("inspect category '{category}' is not a Tier 2 category"),
            };
        }

        let cache = match self.cache_for_snapshot(&snapshot) {
            Ok(cache) => cache,
            Err(message) => return JobOutcome::Failed { message },
        };
        let key = JobKey::for_project_category(category);
        let in_flight = self
            .in_flight
            .lock()
            .map(|guard| guard.contains_key(&key))
            .unwrap_or(false);
        match cache.get_aggregated(&key) {
            Ok(Some(payload)) => filter_outcome_for_scope(
                JobOutcome::Stale {
                    cached: Some(payload),
                    in_flight,
                },
                &caller_scope,
            ),
            Ok(None) => JobOutcome::Pending { in_flight },
            Err(error) => JobOutcome::Failed {
                message: error.to_string(),
            },
        }
    }

    #[doc(hidden)]
    pub fn tier2_run_with_reuse_result(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> InspectResult {
        let started = Instant::now();
        let job = self.tier2_reuse_job(snapshot, category, callgraph_snapshot);

        if !category.is_active() {
            return InspectResult::failed(
                &job,
                format!("inspect category '{category}' is disabled in v0.33"),
                started.elapsed(),
            );
        }
        if !category.is_tier2() {
            return InspectResult::failed(
                &job,
                format!("inspect category '{category}' is not a Tier 2 category"),
                started.elapsed(),
            );
        }

        let cache = match self.cache_for_paths(job.inspect_dir.clone(), job.project_root.clone()) {
            Ok(cache) => cache,
            Err(message) => return InspectResult::failed(&job, message, started.elapsed()),
        };

        match self.tier2_run_with_reuse_job(&job, &cache) {
            Ok(success) => InspectResult::success(&job, success, started.elapsed()),
            Err(message) => InspectResult::failed(&job, message, started.elapsed()),
        }
    }

    fn tier2_reuse_job(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> InspectJob {
        let project_scope = JobScope::for_project(snapshot.project_root.clone());
        let scope_files = scope_files(&snapshot.project_root, &project_scope);
        InspectJob {
            job_id: self.next_job_id.fetch_add(1, Ordering::Relaxed),
            key: JobKey::for_project_category(category),
            category,
            scope_files,
            project_root: snapshot.project_root,
            inspect_dir: snapshot.inspect_dir,
            config: snapshot.config,
            symbol_cache: snapshot.symbol_cache,
            callgraph_snapshot,
        }
    }

    fn tier2_run_with_reuse_job(
        &self,
        job: &InspectJob,
        cache: &InspectCache,
    ) -> Result<InspectScanSuccess, String> {
        let cached_records = cache
            .load_tier2_contributions(job.category)
            .map_err(|error| error.to_string())?;
        let current_by_relative = current_project_files(&job.project_root, &job.scope_files);
        let cached_relative = cached_records
            .iter()
            .map(record_relative_key)
            .collect::<BTreeSet<_>>();

        let mut updates = Tier2ContributionUpdates::default();
        let mut scan_by_relative = BTreeMap::<String, PathBuf>::new();

        for record in cached_records {
            let relative = record_relative_key(&record);
            let relative_path = PathBuf::from(&relative);
            let Some(current_file) = current_by_relative.get(&relative) else {
                updates.deletes.push(relative_path);
                continue;
            };

            let absolute = job.project_root.join(&record.file_path);
            match verify_contribution_file(&absolute, &record.freshness) {
                ContributionFreshness::Fresh {
                    metadata_changed,
                    freshness,
                } => {
                    if metadata_changed {
                        updates.metadata_updates.push((relative_path, freshness));
                    }
                }
                ContributionFreshness::Stale => {
                    updates.deletes.push(relative_path);
                    scan_by_relative.insert(relative, current_file.clone());
                }
                ContributionFreshness::Deleted => {
                    updates.deletes.push(relative_path);
                }
            }
        }

        for (relative, file) in &current_by_relative {
            if !cached_relative.contains(relative) {
                scan_by_relative.insert(relative.clone(), file.clone());
            }
        }

        let scan_files = scan_by_relative.into_values().collect::<Vec<_>>();
        if !scan_files.is_empty() {
            let mut scan_job = job.clone();
            scan_job.job_id = self.next_job_id.fetch_add(1, Ordering::Relaxed);
            scan_job.scope_files = scan_files.clone();
            let scan_result = run_tier2_scan(&scan_job);
            let scan_success = scan_result.outcome.map_err(|message| {
                format!("{} incremental scan failed: {message}", job.category)
            })?;
            updates.upserts.extend(scan_success.contributions);
        }

        let has_updates = !updates.upserts.is_empty()
            || !updates.deletes.is_empty()
            || !updates.metadata_updates.is_empty();
        let contribution_set_hash = if has_updates {
            cache
                .apply_contribution_updates(job.category, updates)
                .map_err(|error| error.to_string())?
        } else {
            cache
                .contribution_set_hash(job.category)
                .map_err(|error| error.to_string())?
        };

        if let Some(aggregate) = cache
            .load_aggregate_if_hash_matches(job.category, &contribution_set_hash)
            .map_err(|error| error.to_string())?
        {
            cache
                .touch_tier2_last_full_run(job.category)
                .map_err(|error| error.to_string())?;
            let contributions = load_contributions(cache, job)?;
            return Ok(InspectScanSuccess {
                scanned_files: scan_files,
                contributions,
                aggregate,
            });
        }

        let contributions = load_contributions(cache, job)?;
        let aggregate = roll_up_tier2_contributions(job, &contributions);
        cache
            .store_tier2_aggregate(job.key.clone(), &contribution_set_hash, aggregate.clone())
            .map_err(|error| error.to_string())?;

        Ok(InspectScanSuccess {
            scanned_files: scan_files,
            contributions,
            aggregate,
        })
    }

    fn enqueue_with_waiter(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        key: JobKey,
        waiter_tx: WaiterTx,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> Result<(), String> {
        let mut in_flight = self
            .in_flight
            .lock()
            .map_err(|_| "inspect in-flight map lock poisoned".to_string())?;
        if let Some(waiters) = in_flight.get_mut(&key) {
            waiters.push(Waiter { tx: waiter_tx });
            return Ok(());
        }

        in_flight.insert(key.clone(), vec![Waiter { tx: waiter_tx }]);
        drop(in_flight);

        if let Err(message) = self.enqueue_new_job(
            snapshot,
            category,
            caller_scope,
            key.clone(),
            callgraph_snapshot,
        ) {
            if let Ok(mut in_flight) = self.in_flight.lock() {
                in_flight.remove(&key);
            }
            return Err(message);
        }
        Ok(())
    }

    fn enqueue_without_waiter(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        key: JobKey,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> Result<(), String> {
        let mut in_flight = self
            .in_flight
            .lock()
            .map_err(|_| "inspect in-flight map lock poisoned".to_string())?;
        if in_flight.contains_key(&key) {
            return Ok(());
        }
        in_flight.insert(key.clone(), Vec::new());
        drop(in_flight);

        if let Err(message) = self.enqueue_new_job(
            snapshot,
            category,
            caller_scope,
            key.clone(),
            callgraph_snapshot,
        ) {
            if let Ok(mut in_flight) = self.in_flight.lock() {
                in_flight.remove(&key);
            }
            return Err(message);
        }
        Ok(())
    }

    fn enqueue_new_job(
        &self,
        snapshot: InspectSnapshot,
        category: InspectCategory,
        caller_scope: JobScope,
        key: JobKey,
        callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
    ) -> Result<(), String> {
        let scan_scope = if category.is_tier2() {
            JobScope::for_project(snapshot.project_root.clone())
        } else {
            caller_scope
        };
        let scope_files = scope_files(&snapshot.project_root, &scan_scope);
        let job = InspectJob {
            job_id: self.next_job_id.fetch_add(1, Ordering::Relaxed),
            key,
            category,
            scope_files,
            project_root: snapshot.project_root,
            inspect_dir: snapshot.inspect_dir,
            config: snapshot.config,
            symbol_cache: snapshot.symbol_cache,
            callgraph_snapshot,
        };
        self.request_tx
            .send(job)
            .map_err(|_| "inspect dispatch loop is unavailable".to_string())
    }

    fn wait_for_outcome(
        &self,
        key: JobKey,
        caller_scope: JobScope,
        cache: Arc<InspectCache>,
        waiter_rx: Receiver<JobOutcome>,
    ) -> JobOutcome {
        let timeout = after(self.soft_deadline);
        let result_rx = self.result_rx.clone();
        loop {
            select! {
                recv(waiter_rx) -> outcome => {
                    return match outcome {
                        Ok(outcome) => filter_outcome_for_scope(outcome, &caller_scope),
                        Err(_) => self.timeout_outcome(&key, &caller_scope, &cache),
                    };
                }
                recv(result_rx) -> result => {
                    match result {
                        Ok(result) => self.route_completion(result),
                        Err(_) => return self.timeout_outcome(&key, &caller_scope, &cache),
                    }
                }
                recv(timeout) -> _ => {
                    return self.timeout_outcome(&key, &caller_scope, &cache);
                }
            }
        }
    }

    fn timeout_outcome(
        &self,
        key: &JobKey,
        caller_scope: &JobScope,
        cache: &InspectCache,
    ) -> JobOutcome {
        match cache.get_aggregated(key) {
            Ok(Some(cached)) => JobOutcome::Stale {
                cached: Some(filter_payload_for_scope(cached, caller_scope)),
                in_flight: true,
            },
            Ok(None) => JobOutcome::Pending { in_flight: true },
            Err(error) => JobOutcome::Failed {
                message: error.to_string(),
            },
        }
    }

    fn route_completion(&self, result: InspectResult) {
        let outcome = self.completion_outcome(result.clone());
        let waiters = self
            .in_flight
            .lock()
            .ok()
            .and_then(|mut in_flight| in_flight.remove(&result.key))
            .unwrap_or_default();
        for waiter in waiters {
            let _ = waiter.tx.send(outcome.clone());
        }
    }

    fn completion_outcome(&self, result: InspectResult) -> JobOutcome {
        let cache =
            match self.cache_for_paths(result.inspect_dir.clone(), result.project_root.clone()) {
                Ok(cache) => cache,
                Err(message) => return JobOutcome::Failed { message },
            };

        match result.outcome {
            Ok(success) => {
                let store_result = if result.category.is_tier2() {
                    cache.store_tier2_result(
                        result.key.clone(),
                        &success.scanned_files,
                        &success.contributions,
                        success.aggregate.clone(),
                    )
                } else {
                    cache.store_aggregated(result.key, success.aggregate.clone())
                };

                match store_result {
                    Ok(()) => JobOutcome::Fresh {
                        payload: success.aggregate,
                    },
                    Err(error) => JobOutcome::Failed {
                        message: error.to_string(),
                    },
                }
            }
            Err(message) => JobOutcome::Failed { message },
        }
    }
}

impl Default for InspectManager {
    fn default() -> Self {
        Self::new()
    }
}

fn scope_files(project_root: &Path, scope: &JobScope) -> Vec<PathBuf> {
    let mut files = crate::callgraph::walk_project_files(project_root)
        .filter(|path| scope.contains(path))
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn current_project_files(project_root: &Path, files: &[PathBuf]) -> BTreeMap<String, PathBuf> {
    files
        .iter()
        .map(|file| (relative_cache_key(project_root, file), file.clone()))
        .collect()
}

fn record_relative_key(record: &super::cache::ContributionRecord) -> String {
    record.file_path.to_string_lossy().to_string()
}

fn relative_cache_key(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn load_contributions(
    cache: &InspectCache,
    job: &InspectJob,
) -> Result<Vec<FileContribution>, String> {
    cache
        .load_tier2_contributions(job.category)
        .map_err(|error| error.to_string())
        .map(|records| {
            records
                .into_iter()
                .map(|record| contribution_from_record(&job.project_root, record))
                .collect()
        })
}

fn contribution_from_record(
    project_root: &Path,
    record: super::cache::ContributionRecord,
) -> FileContribution {
    FileContribution::new(
        record.category,
        project_root.join(record.file_path),
        record.freshness,
        record.contribution,
    )
}

fn run_tier2_scan(job: &InspectJob) -> InspectResult {
    use super::scanners;

    match job.category {
        InspectCategory::DeadCode => scanners::dead_code::run_dead_code_scan(job),
        InspectCategory::UnusedExports => scanners::unused_exports::run_unused_exports_scan(job),
        InspectCategory::Duplicates => scanners::duplicates::run_duplicates_scan(job),
        other => InspectResult::failed(
            job,
            format!("inspect category '{other}' is not an active Tier 2 scanner"),
            Duration::from_secs(0),
        ),
    }
}

fn roll_up_tier2_contributions(job: &InspectJob, contributions: &[FileContribution]) -> Value {
    match job.category {
        InspectCategory::DeadCode => roll_up_dead_code_contributions(job, contributions),
        InspectCategory::UnusedExports => roll_up_unused_exports_contributions(job, contributions),
        InspectCategory::Duplicates => roll_up_duplicate_contributions(job, contributions),
        _ => json!({
            "count": 0,
            "items": [],
            "scanned_files": contributions.len(),
        }),
    }
}

fn roll_up_dead_code_contributions(job: &InspectJob, contributions: &[FileContribution]) -> Value {
    if job.callgraph_snapshot.is_none() {
        return super::scanners::dead_code::callgraph_unavailable_aggregate(job.scope_files.len());
    }

    let public_api_files = super::scanners::dead_code::collect_public_api_files(&job.project_root);
    super::scanners::dead_code::aggregate_dead_code_contributions(contributions, &public_api_files)
}

fn roll_up_unused_exports_contributions(
    job: &InspectJob,
    contributions: &[FileContribution],
) -> Value {
    let parsed = contributions
        .iter()
        .filter_map(|contribution| {
            serde_json::from_value::<UnusedExportsContribution>(contribution.contribution.clone())
                .ok()
        })
        .collect::<Vec<_>>();

    let (public_api_entries, package_warnings) = unused_public_api_entries(&job.project_root);
    let mut imported_by: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let mut wildcard_imported_by: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for scan in &parsed {
        for import in &scan.imports {
            let Some(resolved_file) = &import.resolved_file else {
                continue;
            };
            for name in &import.named {
                if name == "*" {
                    wildcard_imported_by
                        .entry(resolved_file.clone())
                        .or_default()
                        .insert(scan.file.clone());
                } else {
                    imported_by
                        .entry((resolved_file.clone(), name.clone()))
                        .or_default()
                        .insert(scan.file.clone());
                }
            }
        }
    }

    let mut count = 0usize;
    let mut items = Vec::new();
    for scan in &parsed {
        if public_api_entries.contains(&scan.file) {
            continue;
        }

        for export in &scan.exports {
            let imported = imported_by
                .get(&(scan.file.clone(), export.symbol.clone()))
                .map(|files| !files.is_empty())
                .unwrap_or(false);
            let wildcard_imported = wildcard_imported_by
                .get(&scan.file)
                .map(|files| !files.is_empty())
                .unwrap_or(false);

            if imported || wildcard_imported {
                continue;
            }

            count += 1;
            if items.len() < MAX_DRILL_DOWN_ITEMS {
                items.push(json!({
                    "file": scan.file,
                    "symbol": export.symbol,
                    "kind": export.kind,
                    "line": export.line,
                }));
            }
        }
    }

    let mut aggregate = json!({
        "count": count,
        "items": items,
        "drill_down_capped": count > MAX_DRILL_DOWN_ITEMS,
        "scanned_files": parsed.len(),
        "languages_skipped": skipped_languages(&job.scope_files, LanguageSkipMode::UnusedExports),
    });
    if !package_warnings.is_empty() {
        aggregate["note"] = Value::String(package_warnings.join("; "));
    }
    aggregate
}

fn roll_up_duplicate_contributions(job: &InspectJob, contributions: &[FileContribution]) -> Value {
    super::scanners::duplicates::aggregate_duplicate_contributions(
        contributions,
        skipped_languages(&job.scope_files, LanguageSkipMode::Duplicates),
    )
}

const MAX_DRILL_DOWN_ITEMS: usize = 100;
const JS_MODULE_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"];

#[derive(Debug, Clone, Deserialize)]
struct ExportContribution {
    symbol: String,
    kind: String,
    line: u32,
}

#[derive(Debug, Clone, Deserialize)]
struct UnusedExportsContribution {
    file: String,
    exports: Vec<ExportContribution>,
    imports: Vec<ImportContribution>,
}

#[derive(Debug, Clone, Deserialize)]
struct ImportContribution {
    resolved_file: Option<String>,
    named: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
enum LanguageSkipMode {
    Duplicates,
    UnusedExports,
}

fn skipped_languages(files: &[PathBuf], mode: LanguageSkipMode) -> Vec<String> {
    files
        .iter()
        .filter_map(|file| skipped_language(file, mode))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn skipped_language(file: &Path, mode: LanguageSkipMode) -> Option<String> {
    let Some(language) = crate::parser::detect_language(file) else {
        return match mode {
            LanguageSkipMode::Duplicates => Some("unknown".to_string()),
            LanguageSkipMode::UnusedExports => None,
        };
    };

    let skipped = match mode {
        LanguageSkipMode::Duplicates => !duplicates_supports_language(language),
        LanguageSkipMode::UnusedExports => !is_js_ts_language(language),
    };
    skipped.then(|| language_name(language).to_string())
}

fn duplicates_supports_language(language: crate::parser::LangId) -> bool {
    !matches!(
        language,
        crate::parser::LangId::Bash
            | crate::parser::LangId::Html
            | crate::parser::LangId::Json
            | crate::parser::LangId::Scala
            | crate::parser::LangId::Solidity
            | crate::parser::LangId::Vue
            | crate::parser::LangId::Markdown
            | crate::parser::LangId::Java
            | crate::parser::LangId::Ruby
            | crate::parser::LangId::Kotlin
            | crate::parser::LangId::Swift
            | crate::parser::LangId::Php
            | crate::parser::LangId::Lua
            | crate::parser::LangId::Perl
    )
}

fn is_js_ts_language(language: crate::parser::LangId) -> bool {
    matches!(
        language,
        crate::parser::LangId::TypeScript
            | crate::parser::LangId::Tsx
            | crate::parser::LangId::JavaScript
    )
}

fn language_name(language: crate::parser::LangId) -> &'static str {
    match language {
        crate::parser::LangId::TypeScript => "typescript",
        crate::parser::LangId::Tsx => "tsx",
        crate::parser::LangId::JavaScript => "javascript",
        crate::parser::LangId::Python => "python",
        crate::parser::LangId::Rust => "rust",
        crate::parser::LangId::Go => "go",
        crate::parser::LangId::C => "c",
        crate::parser::LangId::Cpp => "cpp",
        crate::parser::LangId::Zig => "zig",
        crate::parser::LangId::CSharp => "csharp",
        crate::parser::LangId::Bash => "bash",
        crate::parser::LangId::Html => "html",
        crate::parser::LangId::Markdown => "markdown",
        crate::parser::LangId::Solidity => "solidity",
        crate::parser::LangId::Vue => "vue",
        crate::parser::LangId::Json => "json",
        crate::parser::LangId::Scala => "scala",
        crate::parser::LangId::Java => "java",
        crate::parser::LangId::Ruby => "ruby",
        crate::parser::LangId::Kotlin => "kotlin",
        crate::parser::LangId::Swift => "swift",
        crate::parser::LangId::Php => "php",
        crate::parser::LangId::Lua => "lua",
        crate::parser::LangId::Perl => "perl",
    }
}

fn unused_public_api_entries(project_root: &Path) -> (BTreeSet<String>, Vec<String>) {
    let mut entries = BTreeSet::new();
    let mut warnings = Vec::new();
    let mut package_jsons = Vec::new();

    let root_package_json = project_root.join("package.json");
    if root_package_json.is_file() {
        package_jsons.push(root_package_json);
    }

    let packages_dir = project_root.join("packages");
    if let Ok(children) = std::fs::read_dir(packages_dir) {
        for child in children.flatten() {
            let package_json = child.path().join("package.json");
            if package_json.is_file() {
                package_jsons.push(package_json);
            }
        }
    }

    for package_json in package_jsons {
        match read_public_entries_from_package_json(&package_json) {
            Ok(package_entries) => {
                let package_dir = package_json.parent().unwrap_or(project_root);
                entries.extend(
                    package_entries
                        .iter()
                        .filter_map(|entry| resolve_package_entry(package_dir, entry))
                        .map(|entry| relative_display_path(project_root, &entry)),
                );
            }
            Err(message) => warnings.push(message),
        }
    }

    (entries, warnings)
}

fn read_public_entries_from_package_json(package_json: &Path) -> Result<Vec<String>, String> {
    let source = std::fs::read_to_string(package_json)
        .map_err(|error| format!("failed to read {}: {error}", package_json.display()))?;
    let value = serde_json::from_str::<Value>(&source)
        .map_err(|error| format!("failed to parse {}: {error}", package_json.display()))?;

    let mut entries = Vec::new();
    if let Some(main) = value.get("main").and_then(Value::as_str) {
        entries.push(main.to_string());
    }
    if let Some(exports) = value.get("exports") {
        collect_package_export_strings(exports, &mut entries);
    }
    Ok(entries)
}

fn collect_package_export_strings(value: &Value, entries: &mut Vec<String>) {
    match value {
        Value::String(entry) => entries.push(entry.clone()),
        Value::Array(values) => {
            for value in values {
                collect_package_export_strings(value, entries);
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                collect_package_export_strings(value, entries);
            }
        }
        _ => {}
    }
}

fn resolve_package_entry(package_dir: &Path, entry: &str) -> Option<PathBuf> {
    if entry.starts_with("node:") || entry.contains("://") {
        return None;
    }

    let entry_path = if is_relative_module(entry) {
        package_dir.join(entry)
    } else {
        package_dir.join(entry.trim_start_matches('/'))
    };
    candidate_paths(&entry_path)
        .into_iter()
        .map(|candidate| normalize_path(&candidate))
        .find(|candidate| candidate.is_file())
}

fn candidate_paths(base: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    candidates.push(base.to_path_buf());

    if base.extension().is_none() {
        for extension in JS_MODULE_EXTENSIONS {
            candidates.push(base.with_extension(extension));
        }
    }

    for extension in JS_MODULE_EXTENSIONS {
        candidates.push(base.join(format!("index.{extension}")));
    }

    candidates
}

fn is_relative_module(module_path: &str) -> bool {
    module_path.starts_with("./")
        || module_path.starts_with("../")
        || module_path == "."
        || module_path == ".."
}

fn relative_display_path(project_root: &Path, path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    };
    let normalized = normalize_path(&absolute);
    normalized
        .strip_prefix(&normalize_path(project_root))
        .unwrap_or(normalized.as_path())
        .to_string_lossy()
        .replace('\\', "/")
}

fn filter_outcome_for_scope(outcome: JobOutcome, scope: &JobScope) -> JobOutcome {
    match outcome {
        JobOutcome::Fresh { payload } => JobOutcome::Fresh {
            payload: filter_payload_for_scope(payload, scope),
        },
        JobOutcome::Stale { cached, in_flight } => JobOutcome::Stale {
            cached: cached.map(|payload| filter_payload_for_scope(payload, scope)),
            in_flight,
        },
        JobOutcome::Pending { in_flight } => JobOutcome::Pending { in_flight },
        JobOutcome::Failed { message } => JobOutcome::Failed { message },
    }
}

fn filter_payload_for_scope(mut payload: serde_json::Value, scope: &JobScope) -> serde_json::Value {
    if scope.is_project_wide() {
        return payload;
    }

    if let Some(items) = payload
        .get_mut("items")
        .and_then(|value| value.as_array_mut())
    {
        items.retain(|item| value_matches_scope(item, scope));
        let count = items.len();
        if let Some(object) = payload.as_object_mut() {
            object.insert("count".to_string(), serde_json::json!(count));
            if object.contains_key("total_groups") {
                object.insert("total_groups".to_string(), serde_json::json!(count));
            }
            if object.contains_key("groups_count") {
                object.insert("groups_count".to_string(), serde_json::json!(count));
            }
        }
    }

    if let Some(groups) = payload
        .get_mut("groups")
        .and_then(|value| value.as_array_mut())
    {
        groups.retain(|group| value_matches_scope(group, scope));
        let count = groups.len();
        if let Some(object) = payload.as_object_mut() {
            object.insert("count".to_string(), serde_json::json!(count));
            object.insert("total_groups".to_string(), serde_json::json!(count));
        }
    }

    payload
}

fn value_matches_scope(value: &serde_json::Value, scope: &JobScope) -> bool {
    if let Some(file) = value.get("file").and_then(|file| file.as_str()) {
        return scope.contains_display_path(file);
    }
    if let Some(files) = value.get("files").and_then(|files| files.as_array()) {
        return files
            .iter()
            .filter_map(|file| file.as_str())
            .any(|file| scope.contains_display_path(display_file_from_occurrence(file)));
    }
    true
}

fn display_file_from_occurrence(value: &str) -> &str {
    let Some((file, range)) = value.rsplit_once(':') else {
        return value;
    };
    let Some((start, end)) = range.split_once('-') else {
        return value;
    };
    if start.chars().all(|char| char.is_ascii_digit())
        && end.chars().all(|char| char.is_ascii_digit())
    {
        file
    } else {
        value
    }
}

#[allow(dead_code)]
fn normalize_scope_root(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| normalize_path(path))
}

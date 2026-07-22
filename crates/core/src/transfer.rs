//! Shared planning and execution primitives for bulk transfers.
//!
//! Commands expand their own storage-specific inputs into [`TransferCandidate`] values, then use
//! this module for consistent selection, ordering, retry, rate, concurrency, cancellation, and
//! aggregate summary behavior.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt as _};
use glob::Pattern;
use jiff::Timestamp;
use serde::Serialize;
use tokio::sync::Mutex;
use tokio::time::{Instant, sleep_until};

use crate::alias::RetryConfig;
use crate::error::{Error, Result};
use crate::retry::{is_retryable_error, retry_with_backoff};

/// A storage-independent item that can be selected and executed as part of a transfer.
#[derive(Debug, Clone)]
pub struct TransferCandidate<T> {
    /// Command-specific operation data.
    pub payload: T,
    /// Display-safe source identity.
    pub source: String,
    /// Display-safe destination identity.
    pub target: String,
    /// Source-relative path used by include and exclude rules.
    pub relative_path: String,
    /// UTC modification timestamp when the source provides one.
    pub modified: Option<Timestamp>,
    /// Expected transfer size, used for aggregate rate pacing.
    pub size_bytes: Option<u64>,
}

/// Shared path and UTC timestamp selection rules.
#[derive(Debug, Clone, Default)]
pub struct TransferSelection {
    include: Vec<Pattern>,
    exclude: Vec<Pattern>,
    newer_than: Option<Timestamp>,
    older_than: Option<Timestamp>,
    rewind: Option<Timestamp>,
}

impl TransferSelection {
    /// Compile a deterministic selection policy.
    ///
    /// Include patterns restrict the candidate set when at least one is present. Exclude patterns
    /// are applied afterwards and always win, independent of command-line flag order. `newer_than`
    /// and `older_than` use strict comparisons; `rewind` includes the boundary itself.
    pub fn new(
        include: &[String],
        exclude: &[String],
        newer_than: Option<Timestamp>,
        older_than: Option<Timestamp>,
        rewind: Option<Timestamp>,
    ) -> Result<Self> {
        let include = compile_patterns("include", include)?;
        let exclude = compile_patterns("exclude", exclude)?;

        Ok(Self {
            include,
            exclude,
            newer_than,
            older_than,
            rewind,
        })
    }

    /// Return whether a candidate satisfies every configured rule.
    pub fn matches<T>(&self, candidate: &TransferCandidate<T>) -> bool {
        let path_matches = (self.include.is_empty()
            || self
                .include
                .iter()
                .any(|pattern| pattern.matches(&candidate.relative_path)))
            && !self
                .exclude
                .iter()
                .any(|pattern| pattern.matches(&candidate.relative_path));

        if !path_matches {
            return false;
        }

        let has_time_filter =
            self.newer_than.is_some() || self.older_than.is_some() || self.rewind.is_some();
        let Some(modified) = candidate.modified else {
            return !has_time_filter;
        };

        self.newer_than.is_none_or(|cutoff| modified > cutoff)
            && self.older_than.is_none_or(|cutoff| modified < cutoff)
            && self.rewind.is_none_or(|cutoff| modified <= cutoff)
    }
}

fn compile_patterns(kind: &str, values: &[String]) -> Result<Vec<Pattern>> {
    values
        .iter()
        .map(|value| {
            Pattern::new(value).map_err(|error| {
                Error::InvalidPath(format!("Invalid {kind} pattern '{value}': {error}"))
            })
        })
        .collect()
}

/// Deterministic aggregate counters for a transfer plan and its execution.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TransferSummary {
    pub planned: usize,
    pub skipped: usize,
    pub successful: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub transferred_bytes: u64,
}

/// Selected candidates in deterministic execution and output order.
#[derive(Debug)]
pub struct TransferPlan<T> {
    pub items: Vec<TransferCandidate<T>>,
    pub summary: TransferSummary,
}

impl<T> TransferPlan<T> {
    /// Filter and sort candidates without performing any transfer.
    pub fn build(candidates: Vec<TransferCandidate<T>>, selection: &TransferSelection) -> Self {
        let candidate_count = candidates.len();
        let mut items: Vec<_> = candidates
            .into_iter()
            .filter(|candidate| selection.matches(candidate))
            .collect();
        items.sort_by(|left, right| {
            left.relative_path
                .cmp(&right.relative_path)
                .then_with(|| left.source.cmp(&right.source))
                .then_with(|| left.target.cmp(&right.target))
        });

        Self {
            summary: TransferSummary {
                planned: items.len(),
                skipped: candidate_count.saturating_sub(items.len()),
                ..TransferSummary::default()
            },
            items,
        }
    }
}

/// Controls applied across the entire transfer command.
#[derive(Debug, Clone)]
pub struct TransferControls {
    pub concurrency: usize,
    pub bytes_per_second: Option<u64>,
    pub retry: RetryConfig,
    pub continue_on_error: bool,
}

impl TransferControls {
    /// Validate global limits before any work starts.
    pub fn validate(&self) -> Result<()> {
        if !(1..=256).contains(&self.concurrency) {
            return Err(Error::Config(
                "Transfer concurrency must be between 1 and 256".to_string(),
            ));
        }
        if self.bytes_per_second == Some(0) {
            return Err(Error::Config(
                "Transfer rate limit must be greater than zero".to_string(),
            ));
        }
        if self.retry.max_attempts == 0 {
            return Err(Error::Config(
                "Transfer retry attempts must be greater than zero".to_string(),
            ));
        }
        if self.retry.initial_backoff_ms > self.retry.max_backoff_ms {
            return Err(Error::Config(
                "Transfer retry initial backoff must not exceed its maximum backoff".to_string(),
            ));
        }
        Ok(())
    }
}

impl Default for TransferControls {
    fn default() -> Self {
        Self {
            concurrency: 4,
            bytes_per_second: None,
            retry: RetryConfig::default(),
            continue_on_error: false,
        }
    }
}

/// Result state for one candidate.
#[derive(Debug)]
pub enum TransferOutcomeState {
    Success { bytes_transferred: u64 },
    Failed { error: Error },
    Cancelled,
}

/// Deterministically ordered result for one candidate.
#[derive(Debug)]
pub struct TransferOutcome<T> {
    pub item: TransferCandidate<T>,
    pub state: TransferOutcomeState,
}

/// Aggregate execution result and per-candidate outcomes.
#[derive(Debug)]
pub struct TransferReport<T> {
    pub summary: TransferSummary,
    pub outcomes: Vec<TransferOutcome<T>>,
}

impl<T> TransferReport<T> {
    /// Return the first failure in deterministic plan order.
    pub fn first_failure(&self) -> Option<&Error> {
        self.outcomes
            .iter()
            .find_map(|outcome| match &outcome.state {
                TransferOutcomeState::Failed { error } => Some(error),
                TransferOutcomeState::Success { .. } | TransferOutcomeState::Cancelled => None,
            })
    }
}

/// Executes a transfer plan with shared controls.
#[derive(Debug, Clone)]
pub struct TransferExecutor {
    controls: TransferControls,
}

impl TransferExecutor {
    pub fn new(controls: TransferControls) -> Result<Self> {
        controls.validate()?;
        Ok(Self { controls })
    }

    /// Execute candidates while bounding all in-flight work across the command.
    pub async fn execute<T, F, Fut>(&self, plan: TransferPlan<T>, operation: F) -> TransferReport<T>
    where
        T: Clone + Send + Sync + 'static,
        F: Fn(TransferCandidate<T>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<u64>> + Send + 'static,
    {
        let mut summary = plan.summary;
        let items = plan.items;
        let operation = Arc::new(operation);
        let rate_gate = Arc::new(RateGate::new(self.controls.bytes_per_second));
        let mut in_flight = FuturesUnordered::new();
        let mut completed = BTreeMap::new();
        let mut next_index = 0usize;
        let mut stop_scheduling = false;

        while next_index < items.len() && in_flight.len() < self.controls.concurrency {
            in_flight.push(execute_candidate(
                next_index,
                items[next_index].clone(),
                Arc::clone(&operation),
                Arc::clone(&rate_gate),
                self.controls.retry.clone(),
            ));
            next_index += 1;
        }

        while let Some((index, result)) = in_flight.next().await {
            let failed = result.is_err();
            completed.insert(index, result);

            if failed && !self.controls.continue_on_error {
                // Work that has already started is allowed to settle so the report never marks a
                // completed side effect as cancelled. No additional candidates are scheduled.
                stop_scheduling = true;
            }

            if !stop_scheduling && next_index < items.len() {
                in_flight.push(execute_candidate(
                    next_index,
                    items[next_index].clone(),
                    Arc::clone(&operation),
                    Arc::clone(&rate_gate),
                    self.controls.retry.clone(),
                ));
                next_index += 1;
            }
        }
        drop(in_flight);

        let mut outcomes = Vec::with_capacity(items.len());
        for (index, item) in items.into_iter().enumerate() {
            let state = match completed.remove(&index) {
                Some(Ok(bytes_transferred)) => {
                    summary.successful += 1;
                    summary.transferred_bytes =
                        summary.transferred_bytes.saturating_add(bytes_transferred);
                    TransferOutcomeState::Success { bytes_transferred }
                }
                Some(Err(error)) => {
                    summary.failed += 1;
                    TransferOutcomeState::Failed { error }
                }
                None => {
                    summary.cancelled += 1;
                    TransferOutcomeState::Cancelled
                }
            };
            outcomes.push(TransferOutcome { item, state });
        }

        TransferReport { summary, outcomes }
    }
}

async fn execute_candidate<T, F, Fut>(
    index: usize,
    item: TransferCandidate<T>,
    operation: Arc<F>,
    rate_gate: Arc<RateGate>,
    retry: RetryConfig,
) -> (usize, Result<u64>)
where
    T: Clone + Send + Sync + 'static,
    F: Fn(TransferCandidate<T>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<u64>> + Send + 'static,
{
    let result = retry_with_backoff(
        &retry,
        || {
            let item = item.clone();
            let operation = Arc::clone(&operation);
            let rate_gate = Arc::clone(&rate_gate);
            async move {
                rate_gate.wait(item.size_bytes.unwrap_or_default()).await;
                operation(item).await
            }
        },
        is_retryable_error,
    )
    .await;
    (index, result)
}

#[derive(Debug)]
struct RateGate {
    bytes_per_second: Option<u64>,
    next_slot: Mutex<Instant>,
}

impl RateGate {
    fn new(bytes_per_second: Option<u64>) -> Self {
        Self {
            bytes_per_second,
            next_slot: Mutex::new(Instant::now()),
        }
    }

    async fn wait(&self, bytes: u64) {
        let Some(bytes_per_second) = self.bytes_per_second else {
            return;
        };
        if bytes == 0 || bytes_per_second == 0 {
            return;
        }

        let wait_until = {
            let mut next_slot = self.next_slot.lock().await;
            let now = Instant::now();
            let wait_until = (*next_slot).max(now);
            let delay = rate_duration(bytes, bytes_per_second);
            *next_slot = wait_until.checked_add(delay).unwrap_or(wait_until);
            wait_until
        };

        if wait_until > Instant::now() {
            sleep_until(wait_until).await;
        }
    }
}

fn rate_duration(bytes: u64, bytes_per_second: u64) -> Duration {
    let seconds = bytes / bytes_per_second;
    let remaining_bytes = bytes % bytes_per_second;
    let nanos =
        (u128::from(remaining_bytes) * 1_000_000_000u128).div_ceil(u128::from(bytes_per_second));
    Duration::from_secs(seconds).saturating_add(Duration::from_nanos(nanos as u64))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn candidate(
        name: &str,
        modified: Option<Timestamp>,
        size_bytes: u64,
    ) -> TransferCandidate<()> {
        TransferCandidate {
            payload: (),
            source: format!("source/{name}"),
            target: format!("target/{name}"),
            relative_path: name.to_string(),
            modified,
            size_bytes: Some(size_bytes),
        }
    }

    #[test]
    fn selection_applies_includes_before_excludes_and_sorts_deterministically() {
        let selection = TransferSelection::new(
            &["reports/*.csv".to_string()],
            &["**/private-*".to_string()],
            None,
            None,
            None,
        )
        .expect("valid selection");
        let plan = TransferPlan::build(
            vec![
                candidate("reports/z.csv", None, 1),
                candidate("reports/private-a.csv", None, 1),
                candidate("notes.txt", None, 1),
                candidate("reports/a.csv", None, 1),
            ],
            &selection,
        );

        assert_eq!(
            plan.items
                .iter()
                .map(|item| item.relative_path.as_str())
                .collect::<Vec<_>>(),
            ["reports/a.csv", "reports/z.csv"]
        );
        assert_eq!(plan.summary.planned, 2);
        assert_eq!(plan.summary.skipped, 2);
    }

    #[test]
    fn time_selection_has_explicit_utc_boundary_semantics() {
        let cutoff: Timestamp = "2026-07-21T12:00:00Z".parse().expect("valid UTC cutoff");
        let at_boundary = candidate("boundary", Some(cutoff), 1);

        let newer = TransferSelection::new(&[], &[], Some(cutoff), None, None)
            .expect("valid newer selection");
        let older = TransferSelection::new(&[], &[], None, Some(cutoff), None)
            .expect("valid older selection");
        let rewind = TransferSelection::new(&[], &[], None, None, Some(cutoff))
            .expect("valid rewind selection");

        assert!(!newer.matches(&at_boundary));
        assert!(!older.matches(&at_boundary));
        assert!(rewind.matches(&at_boundary));
        assert!(!rewind.matches(&candidate("missing", None, 1)));
    }

    #[tokio::test]
    async fn executor_enforces_one_global_concurrency_limit() {
        let controls = TransferControls {
            concurrency: 2,
            ..TransferControls::default()
        };
        let executor = TransferExecutor::new(controls).expect("valid controls");
        let plan = TransferPlan::build(
            (0..6)
                .map(|index| candidate(&format!("{index}"), None, 1))
                .collect(),
            &TransferSelection::default(),
        );
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));

        let report = executor
            .execute(plan, {
                let active = Arc::clone(&active);
                let maximum = Arc::clone(&maximum);
                move |_| {
                    let active = Arc::clone(&active);
                    let maximum = Arc::clone(&maximum);
                    async move {
                        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                        maximum.fetch_max(current, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok(1)
                    }
                }
            })
            .await;

        assert_eq!(maximum.load(Ordering::SeqCst), 2);
        assert_eq!(report.summary.successful, 6);
    }

    #[tokio::test]
    async fn executor_applies_rate_limit_across_all_workers() {
        let controls = TransferControls {
            concurrency: 3,
            bytes_per_second: Some(1_000),
            ..TransferControls::default()
        };
        let executor = TransferExecutor::new(controls).expect("valid controls");
        let plan = TransferPlan::build(
            vec![
                candidate("a", None, 10),
                candidate("b", None, 10),
                candidate("c", None, 10),
            ],
            &TransferSelection::default(),
        );
        let starts = Arc::new(Mutex::new(Vec::new()));

        let report = executor
            .execute(plan, {
                let starts = Arc::clone(&starts);
                move |_| {
                    let starts = Arc::clone(&starts);
                    async move {
                        starts.lock().await.push(Instant::now());
                        Ok(10)
                    }
                }
            })
            .await;

        let starts = starts.lock().await;
        let first = starts.iter().min().copied().expect("first start");
        let last = starts.iter().max().copied().expect("last start");
        assert!(last.duration_since(first) >= Duration::from_millis(15));
        assert_eq!(report.summary.transferred_bytes, 30);
    }

    #[tokio::test]
    async fn executor_retries_only_classified_transient_failures() {
        let controls = TransferControls {
            concurrency: 1,
            retry: RetryConfig {
                max_attempts: 3,
                initial_backoff_ms: 1,
                max_backoff_ms: 1,
            },
            continue_on_error: true,
            ..TransferControls::default()
        };
        let executor = TransferExecutor::new(controls).expect("valid controls");
        let transient_calls = Arc::new(AtomicUsize::new(0));
        let permanent_calls = Arc::new(AtomicUsize::new(0));
        let plan = TransferPlan::build(
            vec![
                candidate("transient", None, 1),
                candidate("permanent", None, 1),
            ],
            &TransferSelection::default(),
        );

        let report = executor
            .execute(plan, {
                let transient_calls = Arc::clone(&transient_calls);
                let permanent_calls = Arc::clone(&permanent_calls);
                move |item| {
                    let transient_calls = Arc::clone(&transient_calls);
                    let permanent_calls = Arc::clone(&permanent_calls);
                    async move {
                        if item.relative_path == "transient" {
                            let attempt = transient_calls.fetch_add(1, Ordering::SeqCst) + 1;
                            if attempt < 3 {
                                return Err(Error::Network("503 Service Unavailable".to_string()));
                            }
                            return Ok(1);
                        }

                        permanent_calls.fetch_add(1, Ordering::SeqCst);
                        Err(Error::Auth("denied".to_string()))
                    }
                }
            })
            .await;

        assert_eq!(transient_calls.load(Ordering::SeqCst), 3);
        assert_eq!(permanent_calls.load(Ordering::SeqCst), 1);
        assert_eq!(report.summary.successful, 1);
        assert_eq!(report.summary.failed, 1);
    }

    #[tokio::test]
    async fn executor_cancels_remaining_work_after_first_failure() {
        let controls = TransferControls {
            concurrency: 1,
            retry: RetryConfig {
                max_attempts: 1,
                ..RetryConfig::default()
            },
            continue_on_error: false,
            ..TransferControls::default()
        };
        let executor = TransferExecutor::new(controls).expect("valid controls");
        let calls = Arc::new(AtomicUsize::new(0));
        let plan = TransferPlan::build(
            vec![
                candidate("a", None, 1),
                candidate("b", None, 1),
                candidate("c", None, 1),
            ],
            &TransferSelection::default(),
        );

        let report = executor
            .execute(plan, {
                let calls = Arc::clone(&calls);
                move |_| {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Err(Error::Auth("stop".to_string()))
                    }
                }
            })
            .await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(report.summary.failed, 1);
        assert_eq!(report.summary.cancelled, 2);
    }

    #[tokio::test]
    async fn executor_drains_started_work_but_does_not_schedule_more_after_failure() {
        let controls = TransferControls {
            concurrency: 2,
            retry: RetryConfig {
                max_attempts: 1,
                ..RetryConfig::default()
            },
            continue_on_error: false,
            ..TransferControls::default()
        };
        let executor = TransferExecutor::new(controls).expect("valid controls");
        let calls = Arc::new(AtomicUsize::new(0));
        let plan = TransferPlan::build(
            vec![
                candidate("a", None, 1),
                candidate("b", None, 1),
                candidate("c", None, 1),
            ],
            &TransferSelection::default(),
        );

        let report = executor
            .execute(plan, {
                let calls = Arc::clone(&calls);
                move |item| {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        if item.relative_path == "a" {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                            Err(Error::Auth("stop".to_string()))
                        } else {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            Ok(1)
                        }
                    }
                }
            })
            .await;

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(report.summary.successful, 1);
        assert_eq!(report.summary.failed, 1);
        assert_eq!(report.summary.cancelled, 1);
    }
}

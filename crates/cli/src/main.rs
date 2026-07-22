//! rc - Rust S3 CLI Client
//!
//! A command-line interface for S3-compatible object storage services.
//! Designed for RustFS and other S3-compatible backends.

use clap::Parser;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

mod commands;
mod exit_code;
mod output;

use commands::Cli;
use exit_code::ExitCode;

const COMMAND_THREAD_NAME: &str = "rc-command";
// AWS SDK command futures can exceed the Windows process main-thread stack. A 16 MiB
// worker stack provides headroom for the largest transfer commands observed in CI.
const COMMAND_THREAD_STACK_SIZE: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
enum LauncherError {
    #[error("Failed to start command thread: {0}")]
    ThreadSpawn(#[source] std::io::Error),

    #[error("Failed to initialize async runtime: {0}")]
    Runtime(#[source] std::io::Error),

    #[error("Command execution thread terminated unexpectedly")]
    ThreadPanicked,
}

fn build_env_filter(debug: bool, env_is_set: bool) -> EnvFilter {
    if debug && !env_is_set {
        EnvFilter::new("debug")
    } else {
        EnvFilter::from_default_env()
    }
}

fn main() {
    let cli = Cli::parse();
    let env_is_set = std::env::var_os(EnvFilter::DEFAULT_ENV).is_some();
    let env_filter = build_env_filter(cli.debug, env_is_set);

    // Initialize tracing subscriber for logging
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(env_filter)
        .init();

    let exit_code = match run_command(cli) {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::GeneralError
        }
    };

    std::process::exit(exit_code.as_i32());
}

fn run_command(cli: Cli) -> Result<ExitCode, LauncherError> {
    run_async_command(move || commands::execute(cli), command_runtime)
}

fn command_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .thread_stack_size(COMMAND_THREAD_STACK_SIZE)
        .enable_all()
        .build()
}

fn run_async_command<T, F, Fut, R>(
    future_factory: F,
    runtime_factory: R,
) -> Result<T, LauncherError>
where
    T: Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
    R: FnOnce() -> std::io::Result<tokio::runtime::Runtime> + Send + 'static,
{
    run_on_command_thread(move || {
        let runtime = runtime_factory().map_err(LauncherError::Runtime)?;
        let output = runtime.block_on(future_factory());
        // The former async main exited the process without waiting for runtime workers.
        // Preserve that bounded shutdown behavior if a background task is wedged.
        runtime.shutdown_background();
        Ok(output)
    })
}

fn run_on_command_thread<T, F>(command: F) -> Result<T, LauncherError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, LauncherError> + Send + 'static,
{
    // The Windows process main thread has a smaller stack than the async command graph
    // requires. Constructing and polling the command future here keeps it off that stack.
    let command_thread = std::thread::Builder::new()
        .name(COMMAND_THREAD_NAME.to_string())
        .stack_size(COMMAND_THREAD_STACK_SIZE)
        .spawn(command)
        .map_err(LauncherError::ThreadSpawn)?;

    command_thread
        .join()
        .map_err(|_| LauncherError::ThreadPanicked)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::sync_channel;
    use std::time::Duration;

    fn test_runtime() -> std::io::Result<tokio::runtime::Runtime> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
    }

    #[test]
    fn debug_uses_debug_filter_when_env_missing() {
        let filter = build_env_filter(true, false);
        assert_eq!(filter.to_string(), "debug");
    }

    #[test]
    fn command_launcher_preserves_command_exit_code() {
        let exit_code = run_async_command(|| async { ExitCode::UnsupportedFeature }, test_runtime)
            .expect("command launcher should return its exit code");

        assert_eq!(exit_code, ExitCode::UnsupportedFeature);
    }

    #[test]
    fn command_launcher_uses_the_named_worker_thread() {
        let thread_name = run_async_command(
            || async { std::thread::current().name().map(str::to_string) },
            test_runtime,
        )
        .expect("command launcher should return its worker thread name");

        assert_eq!(thread_name.as_deref(), Some(COMMAND_THREAD_NAME));
        assert_eq!(COMMAND_THREAD_STACK_SIZE, 16 * 1024 * 1024);
    }

    #[test]
    fn command_launcher_constructs_and_polls_large_future_on_worker() {
        const LARGE_FUTURE_SIZE: usize = 2 * 1024 * 1024;

        async fn large_future() -> usize {
            let marker = std::hint::black_box(7_u8);
            let payload = [marker; LARGE_FUTURE_SIZE];
            tokio::task::yield_now().await;
            let payload = std::hint::black_box(&payload);
            usize::from(payload[0]) + usize::from(payload[LARGE_FUTURE_SIZE - 1])
        }

        let result = run_async_command(large_future, test_runtime)
            .expect("large command future should complete on the sized worker stack");

        assert_eq!(result, 14);
    }

    #[test]
    fn command_runtime_polls_spawned_large_future_on_sized_worker() {
        const LARGE_FUTURE_SIZE: usize = 1024 * 1024;

        async fn large_future() -> usize {
            let marker = std::hint::black_box(11_u8);
            let payload = [marker; LARGE_FUTURE_SIZE];
            tokio::task::yield_now().await;
            let payload = std::hint::black_box(&payload);
            usize::from(payload[0]) + usize::from(payload[LARGE_FUTURE_SIZE - 1])
        }

        let result = run_async_command(
            || async {
                tokio::spawn(large_future())
                    .await
                    .expect("spawned large future should complete")
            },
            command_runtime,
        )
        .expect("command launcher should use the production runtime");

        assert_eq!(result, 22);
    }

    #[test]
    fn command_launcher_does_not_wait_for_blocking_tasks_on_shutdown() {
        let (started_tx, started_rx) = sync_channel(0);
        let (release_tx, release_rx) = sync_channel(0);
        let (result_tx, result_rx) = sync_channel(1);
        let launcher = std::thread::spawn(move || {
            let result = run_async_command(
                move || async move {
                    let _task = tokio::task::spawn_blocking(move || {
                        started_tx
                            .send(())
                            .expect("test should observe the blocking task start");
                        release_rx
                            .recv()
                            .expect("test should release the blocking task");
                    });
                    started_rx
                        .recv()
                        .expect("blocking task should report that it started");
                    ExitCode::Success
                },
                command_runtime,
            );
            result_tx
                .send(result)
                .expect("test should receive the launcher result");
        });

        let result = result_rx.recv_timeout(Duration::from_secs(2));
        release_tx
            .send(())
            .expect("blocking task should remain available for cleanup");
        launcher.join().expect("launcher test thread should finish");

        let exit_code = result
            .expect("command launcher should not join blocking runtime tasks")
            .expect("command launcher should return a successful result");
        assert_eq!(exit_code, ExitCode::Success);
    }

    #[test]
    fn command_launcher_maps_runtime_creation_failure() {
        let future_created = Arc::new(AtomicBool::new(false));
        let future_created_by_worker = Arc::clone(&future_created);

        let error = run_async_command(
            move || {
                future_created_by_worker.store(true, Ordering::SeqCst);
                async {}
            },
            || Err(std::io::Error::other("expected runtime failure")),
        )
        .expect_err("runtime creation failure should be returned by the launcher");

        assert!(matches!(error, LauncherError::Runtime(_)));
        assert_eq!(
            error.to_string(),
            "Failed to initialize async runtime: expected runtime failure"
        );
        assert!(!future_created.load(Ordering::SeqCst));
    }

    #[test]
    fn command_launcher_has_stable_thread_spawn_failure_mapping() {
        let error = LauncherError::ThreadSpawn(std::io::Error::other("expected spawn failure"));

        assert_eq!(
            error.to_string(),
            "Failed to start command thread: expected spawn failure"
        );
    }

    #[test]
    fn command_launcher_maps_worker_panic_to_stable_error() {
        let error = run_on_command_thread::<(), _>(|| panic!("expected worker panic"))
            .expect_err("worker panic should be returned by the launcher");

        assert!(matches!(error, LauncherError::ThreadPanicked));
        assert_eq!(
            error.to_string(),
            "Command execution thread terminated unexpectedly"
        );
        assert!(!error.to_string().contains("expected worker panic"));
    }
}

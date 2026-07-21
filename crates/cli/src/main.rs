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

    install_command_panic_hook();

    let exit_code = match run_command(cli) {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::GeneralError
        }
    };

    std::process::exit(exit_code.as_i32());
}

fn install_command_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let current_thread = std::thread::current();
        if current_thread.name() != Some(COMMAND_THREAD_NAME) {
            default_hook(panic_info);
        }
    }));
}

fn run_command(cli: Cli) -> Result<ExitCode, LauncherError> {
    run_async_command(
        move || commands::execute(cli),
        || {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
        },
    )
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
        Ok(runtime.block_on(future_factory()))
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
    fn command_launcher_maps_worker_panic_without_exposing_payload() {
        let error = run_on_command_thread::<(), _>(|| {
            panic!("sensitive panic payload must not be propagated")
        })
        .expect_err("worker panic should be returned by the launcher");

        assert!(matches!(error, LauncherError::ThreadPanicked));
        assert_eq!(
            error.to_string(),
            "Command execution thread terminated unexpectedly"
        );
        assert!(!error.to_string().contains("sensitive panic payload"));
    }
}

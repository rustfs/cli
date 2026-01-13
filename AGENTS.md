# AGENTS.md - AI Development Guidelines

This document defines the standards and constraints that AI assistants must follow when developing the rc project.

## Language Requirements

**Important**: All content must be in English, except for conversations with AI assistants which may use Chinese:

- Code comments
- Commit messages
- PR titles and descriptions
- Documentation in code
- Error messages in code
- Log messages
- Variable and function names

## Workflow

### 1. Before Starting

1. Read `IMPLEMENTATION_PLAN.md` to confirm the current phase
2. Check if the current phase status is "In Progress"
3. Understand the goals and acceptance criteria for the current phase

### 2. Before Modifying

1. Check if the target file is a protected file
2. If it's a protected file, follow the Breaking Change process
3. Read related existing code to understand patterns and conventions

### 3. During Implementation

1. Write tests first (red)
2. Implement minimum code to pass tests (green)
3. Refactor and clean up code
4. Ensure all tests pass

### 4. After Completion

**⚠️ Important: All checks must pass before each commit!**

1. Run `cargo fmt --all` - Code formatting
2. Run `cargo clippy --workspace -- -D warnings` - Static analysis, **must have zero warnings**
3. Run `cargo test --workspace` - Unit tests, **must all pass**
4. Update `IMPLEMENTATION_PLAN.md` status
5. **Only create a git commit after all above checks pass**
   - Commit message format: `feat(phase-N): <description>`
   - Example: `feat(phase-1): implement alias commands and core infrastructure`

**Do not commit code when checks have not passed!**

---

## Protected Files

The following files require the Breaking Change process for modifications:

| File | Description |
|------|-------------|
| `docs/SPEC.md` | CLI behavior contract |
| `schemas/output_v1.json` | JSON output schema |
| `crates/cli/src/exit_code.rs` | Exit code definitions |
| `crates/core/src/config.rs` | Config schema_version related |

### Breaking Change Process

Modifying protected files requires:

1. **Update version number**
   - Config changes: bump `schema_version`
   - Output changes: create new `output_v2.json` schema

2. **Provide migration path**
   - Config migration: add `migrations/v{N}_to_v{N+1}.rs`
   - Documentation update: update relevant SPEC.md sections

3. **Update CHANGELOG**
   - Add BREAKING CHANGE entry in CHANGELOG.md

4. **PR marking**
   - Include `BREAKING` in PR title or description

---

## Absolute Prohibitions

### Code Level

1. **Using `use aws_sdk_s3` directly in `cli` crate**
   - Must access S3 functionality through `core` trait abstractions
   - Violation: breaks dependency boundaries

2. **Using `.unwrap()`** (except in test code)
   - Must use `?` or `expect("reason")`
   - Violation: may cause panic

3. **Using `unsafe` code**
   - No exceptions
   - Violation: security risk

4. **Printing credentials in logs/errors**
   - Includes: access_key, secret_key, Authorization headers
   - Violation: security risk

### Process Level

5. **Modifying protected files without following Breaking Change process**
   - Violation: breaks backward compatibility

6. **Deleting or disabling tests to "fix" CI**
   - Must fix the root cause of test failures
   - Violation: reduces code quality

7. **Cross-layer refactoring without ADR**
   - Example: moving s3 logic to cli
   - Requires recording decision in `docs/ADR/`

8. **Using `--no-verify` to bypass commit hooks**
   - No exceptions

---

## Code Style

### Error Handling

```rust
// Recommended: Use thiserror to define error types
use thiserror::Error;

#[derive(Error, Debug)]
pub enum MyError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Network error: {0}")]
    Network(#[from] std::io::Error),
}

// Recommended: Use ? to propagate errors
fn do_something() -> Result<()> {
    let config = load_config()?;
    // ...
    Ok(())
}

// Forbidden: bare unwrap
fn bad() {
    let x = something().unwrap(); // ❌
}

// Allowed: expect with explanation (only when failure is impossible)
fn ok() {
    let x = "123".parse::<i32>().expect("hardcoded valid number"); // ✓
}
```

### Async Code

```rust
// Recommended: Use tokio
use tokio;

#[tokio::main]
async fn main() {
    // ...
}

// Forbidden: Using block_on in async context
fn bad() {
    tokio::runtime::Runtime::new().unwrap().block_on(async_fn()); // ❌
}
```

### Logging

```rust
use tracing::{debug, info, warn, error};

// Recommended: Use appropriate log levels
debug!("Detailed debug info");
info!("Normal operation info");
warn!("Warning message");
error!("Error message");

// Forbidden: Logging sensitive information
error!("Auth failed: key={}", secret_key); // ❌ Absolutely forbidden
error!("Auth failed: endpoint={}", endpoint); // ✓ Non-sensitive info is OK
```

### Comments

```rust
// Recommended: Comments explain WHY, not just WHAT

// Using path-style addressing because some S3-compatible services
// don't support virtual-hosted style
let client = S3Client::new_with_path_style();

// Forbidden: Obvious comments
// Increment counter
counter += 1; // ❌ This comment adds no value
```

---

## Command Implementation Template

New commands must follow this template:

```rust
// crates/cli/src/commands/example.rs

use crate::exit_code::ExitCode;
use crate::output::Formatter;
use core::{ObjectStore, Result};

/// Command description (shown in --help)
#[derive(clap::Args, Debug)]
pub struct ExampleArgs {
    /// Argument description
    #[arg(short, long)]
    pub flag: bool,

    /// Path argument
    pub path: String,
}

/// Execute the command
pub async fn execute(
    args: ExampleArgs,
    store: &dyn ObjectStore,
    formatter: &Formatter,
) -> ExitCode {
    match do_work(args, store).await {
        Ok(result) => {
            formatter.output(&result);
            ExitCode::Success
        }
        Err(e) => {
            formatter.error(&e.to_string());
            ExitCode::from_error(&e)
        }
    }
}

async fn do_work(args: ExampleArgs, store: &dyn ObjectStore) -> Result<OutputType> {
    // Implementation logic
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_example_success() {
        // Test success scenario
        // assert_eq!(result.exit_code, ExitCode::Success);
    }

    #[tokio::test]
    async fn test_example_not_found() {
        // Test NOT_FOUND scenario
        // assert_eq!(result.exit_code, ExitCode::NotFound);
    }

    // Each command needs at least 2 exit code tests
}
```

---

## PR Checklist

**⚠️ All checklist items must pass before each commit:**

Before submitting a PR, confirm all of the following:

- [ ] `cargo fmt --all --check` passes (**required**)
- [ ] `cargo clippy --workspace -- -D warnings` passes (**required, zero warnings**)
- [ ] `cargo test --workspace` passes (**required, all must pass**)
- [ ] No changes to CLI/JSON/config contracts (or followed Breaking Change process)
- [ ] New behaviors have unit tests
- [ ] Each new command has at least 2 exit code test scenarios
- [ ] Golden tests pass (output schema not broken)
- [ ] No sensitive information in logs
- [ ] Complex logic has appropriate comments (explaining WHY)
- [ ] Updated IMPLEMENTATION_PLAN.md status (if applicable)
- [ ] Commit message and PR description are in English

**Do not skip checks and commit directly! Code that fails CI should not be merged.**

---

## Dependency Boundaries

```
┌─────────────────────────────────────────────────────────┐
│                         cli                             │
│  (command handling, output formatting, progress bar)    │
│                                                         │
│  ✓ Can depend on: core                                  │
│  ✗ Cannot depend on: s3, aws-sdk-*                      │
└─────────────────────────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────┐
│                        core                              │
│  (config, alias, path parsing, ObjectStore trait)       │
│                                                         │
│  ✓ Can depend on: std, serde, tokio, etc.               │
│  ✗ Cannot depend on: aws-sdk-*                          │
└─────────────────────────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────┐
│                         s3                               │
│  (aws-sdk-s3 wrapper, implements ObjectStore trait)     │
│                                                         │
│  ✓ Can depend on: core, aws-sdk-s3                      │
└─────────────────────────────────────────────────────────┘
```

---

## Common Errors and Fixes

### Compilation Errors

| Error | Wrong Approach | Correct Approach |
|-------|----------------|------------------|
| Type mismatch | Delete the code | Understand types, convert correctly |
| Missing trait | Comment out the call | Implement or import required trait |
| Lifetime error | Add `'static` | Analyze ownership, use Clone or refactor |

### Test Failures

| Situation | Wrong Approach | Correct Approach |
|-----------|----------------|------------------|
| Test hangs | `#[ignore]` | Analyze cause, fix code or test |
| Assertion fails | Change the assertion | Check if it's code error or test error |
| Timeout | Delete the test | Optimize code or increase timeout limit |

---

## Phase Work Guidelines

### Phase 0: Project Initialization
- Create workspace structure
- Set up CI configuration
- Create documentation skeleton

### Phase 1: Core Infrastructure
- Implement exit codes and error types
- Implement config and Alias management
- Implement path parsing
- Implement ObjectStore trait
- Implement alias command

### Phase 2+: Command Implementation
- Follow command implementation template
- Each command needs tests
- Update SPEC.md (if needed)
- Update Golden tests

### Git Commit Guidelines

Each completed phase should have a commit:
- Format: `feat(phase-N): <brief description>`
- Example: `feat(phase-0): initialize project structure and CI`
- Example: `feat(phase-1): implement core infrastructure and alias commands`

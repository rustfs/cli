# AGENTS.md - AI 开发规范

本文档定义了 AI 助手在开发 rc 项目时必须遵循的规范和约束。

## 语言规范

**重要**：除了与 AI 助手的对话可以使用中文外，所有其他内容必须使用英文：

- Code comments (代码注释)
- Commit messages (提交消息)
- PR titles and descriptions (PR 标题和描述)
- Documentation in code (代码文档)
- Error messages in code (代码中的错误消息)
- Log messages (日志消息)
- Variable and function names (变量和函数名)

## 工作流程

### 1. 开始前

1. 读取 `IMPLEMENTATION_PLAN.md` 确认当前阶段
2. 检查当前阶段的状态是否为 "进行中"
3. 理解当前阶段的目标和验收标准

### 2. 修改前

1. 检查目标文件是否为受保护文件
2. 如果是受保护文件，必须遵循 Breaking Change 流程
3. 阅读相关的现有代码，理解模式和约定

### 3. 实现时

1. 先写测试（红灯）
2. 实现最少代码通过测试（绿灯）
3. 重构清理代码
4. 确保所有测试通过

### 4. 完成后

1. 运行 `cargo fmt --all`
2. 运行 `cargo clippy --workspace -- -D warnings`
3. 运行 `cargo test --workspace`
4. 更新 `IMPLEMENTATION_PLAN.md` 状态
5. **每完成一个阶段，创建一次 git commit**
   - Commit message format: `feat(phase-N): <description>`
   - Example: `feat(phase-1): implement alias commands and core infrastructure`

---

## 受保护文件

以下文件的修改需要 Breaking Change 流程：

| 文件 | 说明 |
|------|------|
| `docs/SPEC.md` | CLI 行为合同 |
| `schemas/output_v1.json` | JSON 输出 schema |
| `crates/cli/src/exit_code.rs` | 退出码定义 |
| `crates/core/src/config.rs` | 配置 schema_version 相关 |

### Breaking Change 流程

修改受保护文件必须同时：

1. **更新版本号**
   - 配置变更：bump `schema_version`
   - 输出变更：创建新的 `output_v2.json` schema

2. **提供迁移方案**
   - 配置迁移：添加 `migrations/v{N}_to_v{N+1}.rs`
   - 文档更新：更新 SPEC.md 相关章节

3. **更新 CHANGELOG**
   - 在 CHANGELOG.md 中添加 BREAKING CHANGE 条目

4. **PR 标记**
   - 在 PR 标题或描述中包含 `BREAKING`

---

## 绝对禁止

### 代码层面

1. **在 `cli` crate 中直接 `use aws_sdk_s3`**
   - 必须通过 `core` 的 trait 抽象访问 S3 功能
   - 违反：破坏依赖边界

2. **使用 `.unwrap()`**（测试代码除外）
   - 必须使用 `?` 或 `expect("reason")`
   - 违反：可能导致 panic

3. **使用 `unsafe` 代码**
   - 无例外
   - 违反：安全风险

4. **在日志/错误中打印凭证信息**
   - 包括：access_key, secret_key, Authorization 头
   - 违反：安全风险

### 流程层面

5. **修改受保护文件而不走 Breaking Change 流程**
   - 违反：破坏向后兼容性

6. **删除或禁用测试来"修复" CI**
   - 必须修复测试失败的根本原因
   - 违反：降低代码质量

7. **跨层重构未经 ADR**
   - 例如：把 s3 逻辑移到 cli
   - 需要在 `docs/ADR/` 中记录决策

8. **使用 `--no-verify` 绕过 commit hooks**
   - 无例外

---

## 代码风格

### 错误处理

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

### 异步代码

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

### 日志

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

### 注释

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

## 命令实现模板

新命令必须遵循此模板：

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

Before submitting a PR, confirm all of the following:

- [ ] No changes to CLI/JSON/config contracts (or followed Breaking Change process)
- [ ] New behaviors have unit tests
- [ ] Each new command has at least 2 exit code test scenarios
- [ ] Golden tests pass (output schema not broken)
- [ ] No sensitive information in logs
- [ ] `cargo fmt --all --check` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] Complex logic has appropriate comments (explaining WHY)
- [ ] Updated IMPLEMENTATION_PLAN.md status (if applicable)
- [ ] Commit message and PR description are in English

---

## 依赖边界 (Dependency Boundaries)

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

## 常见错误及修复 (Common Errors and Fixes)

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

## 阶段工作指南 (Phase Work Guidelines)

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


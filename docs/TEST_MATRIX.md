# rc 测试矩阵

本文档定义了 rc CLI 的测试策略和兼容性矩阵。

## 兼容性矩阵

### 后端服务

| 级别 | 服务 | 版本 | CI 状态 | 说明 |
|------|------|------|---------|------|
| Tier 1 | RustFS | latest | 每次 PR | 主要目标，必须 100% 通过 |
| Tier 2 | MinIO | latest | 每次 PR | 完全支持，必须 100% 通过 |
| Tier 3 | AWS S3 | - | 每周 | 尽力支持，定期测试 |
| Best Effort | 其他 | - | 手动 | 不保证，欢迎 PR |

### 命令分组

| 分组 | 命令 | 级别 |
|------|------|------|
| basic | ls, mb, rb, cat, head, stat | core |
| transfer | cp, mv, rm, pipe | core |
| advanced | find, diff, mirror, tree, share | core |
| optional | version, retention, tag, watch, sql | optional |

## 测试层级

### 1. 单元测试 (Unit Tests)

**运行时机**: 每次提交

**覆盖范围**:
- 路径解析 (`core::path`)
- 配置管理 (`core::config`)
- 别名管理 (`core::alias`)
- 错误映射 (`core::error`)
- 退出码 (`cli::exit_code`)
- 分页合并逻辑
- 重试策略

**命令**:
```bash
cargo test --workspace
```

### 2. 集成测试 (Integration Tests)

**运行时机**: 每次 PR

**覆盖范围**:
- 命令行解析和验证
- 退出码验证（每个命令至少 2 个场景）
- JSON 输出格式验证
- 配置文件读写

**命令**:
```bash
cargo test --workspace --features integration
```

### 3. 端到端测试 (E2E Tests)

**运行时机**: 每日 + Release

**环境**: Docker Compose (RustFS + MinIO)

**覆盖范围**:
- 完整命令流程
- 多后端兼容性
- 大文件传输
- 断点续传
- 并发操作

**命令**:
```bash
docker compose -f docker/docker-compose.yml up -d
cargo test --workspace --features e2e
docker compose -f docker/docker-compose.yml down
```

## Golden Tests

Golden tests 用于验证 JSON 输出格式不发生意外变化。

### 目录结构

```
tests/
├── golden/
│   ├── ls_bucket.json
│   ├── ls_objects.json
│   ├── stat_object.json
│   ├── alias_list.json
│   └── ...
└── golden.rs
```

### 更新 Golden 文件

当输出格式有意变更时：

```bash
# 更新所有 golden 文件
UPDATE_GOLDEN=1 cargo test --features golden

# 更新特定 golden 文件
UPDATE_GOLDEN=1 cargo test golden::test_ls_output
```

### CI 验证

```bash
cargo test --features golden
```

## 退出码测试

每个命令必须至少测试 2 个退出码场景：

```rust
#[test]
fn test_ls_success() {
    let result = run_command(&["ls", "minio/bucket"]);
    assert_eq!(result.exit_code, 0);
}

#[test]
fn test_ls_bucket_not_found() {
    let result = run_command(&["ls", "minio/nonexistent"]);
    assert_eq!(result.exit_code, 5); // NOT_FOUND
}

#[test]
fn test_ls_invalid_path() {
    let result = run_command(&["ls", "invalid"]);
    assert_eq!(result.exit_code, 2); // USAGE_ERROR
}
```

## CI 配置

### 基础 CI (ci.yml)

```yaml
on: [push, pull_request]
jobs:
  test:
    - cargo fmt --check
    - cargo clippy -- -D warnings
    - cargo test --workspace
    - cargo test --features integration
```

### 兼容性矩阵 (compat-matrix.yml)

```yaml
on:
  schedule:
    - cron: '0 2 * * *'  # 每天凌晨 2 点
  workflow_dispatch:

jobs:
  e2e:
    strategy:
      matrix:
        backend: [rustfs, minio]
        command_group: [basic, transfer, advanced]
```

## 本地测试

### 快速测试

```bash
# 格式检查
cargo fmt --all --check

# Lint
cargo clippy --workspace -- -D warnings

# 单元测试
cargo test --workspace

# 特定测试
cargo test --package rc-core test_path_parsing
```

### 完整测试

```bash
# 启动测试环境
docker compose -f docker/docker-compose.yml up -d

# 运行所有测试
cargo test --workspace --all-features

# 清理
docker compose -f docker/docker-compose.yml down -v
```

## 测试工具

### 模拟 S3 后端

对于单元测试和集成测试，使用 `mockall` 创建模拟的 `ObjectStore`:

```rust
use mockall::mock;

mock! {
    pub ObjectStore {}

    #[async_trait]
    impl ObjectStore for ObjectStore {
        async fn list_buckets(&self) -> Result<Vec<ObjectInfo>>;
        // ...
    }
}
```

### 测试辅助函数

```rust
// tests/common/mod.rs

pub fn setup_test_alias() -> Alias {
    Alias::new("test", "http://localhost:9000", "minioadmin", "minioadmin")
}

pub async fn setup_test_bucket(client: &S3Client, name: &str) {
    client.create_bucket(name).await.unwrap();
}

pub fn run_command(args: &[&str]) -> CommandResult {
    // ...
}
```

## 覆盖率

目标覆盖率：

| 模块 | 目标 |
|------|------|
| core | >= 80% |
| cli/commands | >= 70% |
| s3 | >= 60% |

生成覆盖率报告：

```bash
cargo tarpaulin --workspace --out html
```


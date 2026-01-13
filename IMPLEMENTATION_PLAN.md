# rc 实施计划

本文档跟踪 rc CLI 项目的实施进度。

## 当前状态

**当前阶段**: 阶段 2 - 基础命令
**开始日期**: 2026-01-13
**阶段 0 完成**: 2026-01-13
**阶段 1 完成**: 2026-01-13

---

## 阶段 0: 项目初始化

**目标**: 建立可编译、可测试的项目骨架

**状态**: ✅ 已完成

### 交付物

| 项目 | 状态 | 说明 |
|------|------|------|
| Workspace 结构 | ✅ 完成 | Cargo.toml + crates/ |
| crates/cli | ✅ 完成 | main.rs, lib.rs, exit_code.rs |
| crates/core | ✅ 完成 | lib.rs, error.rs, config.rs, alias.rs, path.rs, traits.rs |
| crates/s3 | ✅ 完成 | lib.rs, client.rs, capability.rs |
| CI 配置 | ✅ 完成 | .github/workflows/ci.yml |
| Release 配置 | ✅ 完成 | .github/workflows/release.yml |
| AGENTS.md | ✅ 完成 | AI 开发规范 |
| SPEC.md | ✅ 完成 | CLI 行为合同 |
| schemas/ | ✅ 完成 | output_v1.json |
| TEST_MATRIX.md | ✅ 完成 | 测试策略 |
| docker-compose.yml | ✅ 完成 | E2E 测试环境 |
| README.md | ✅ 完成 | 项目说明 |

### 验收标准

- [x] `cargo build --workspace` 成功
- [x] `cargo test --workspace` 通过
- [x] `cargo fmt --all --check` 无警告
- [x] `cargo clippy --workspace -- -D warnings` 无警告
- [ ] CI 流水线绿色 (需要推送代码后验证)

---

## 阶段 1: 核心基础设施

**目标**: 实现配置管理、Alias、路径解析和 ObjectStore trait

**状态**: ✅ 已完成

**完成时间**: 2026-01-13

### 交付物

| 项目 | 状态 | 说明 |
|------|------|------|
| 退出码定义 + 测试 | ✅ 完成 | 9 个退出码，完整测试 |
| 错误类型定义 | ✅ 完成 | thiserror 错误类型 |
| 配置管理 | ✅ 完成 | TOML 格式，schema_version 支持 |
| Alias 管理 | ✅ 完成 | 完整 CRUD 操作 |
| 路径解析器 | ✅ 完成 | 本地/远程路径解析 |
| ObjectStore trait | ✅ 完成 | 异步 trait 定义 |
| S3 客户端封装 | ✅ 完成 | aws-sdk-s3 封装 |
| `alias set` 命令 | ✅ 完成 | 完整实现 + 验证 |
| `alias list` 命令 | ✅ 完成 | human + JSON 输出 |
| `alias remove` 命令 | ✅ 完成 | 正确退出码 |

### 验收标准 (已通过)

```bash
rc alias set minio http://localhost:9000 minioadmin minioadmin  # ✅
rc alias list --json  # ✅ 验证 JSON schema
rc alias remove minio  # ✅
echo $?  # ✅ 退出码 0
rc alias remove nonexistent  # ✅ 退出码 5 (NOT_FOUND)
```

---

## 阶段 2: 基础命令

**目标**: 实现最常用的基础操作命令

**状态**: ⏳ 待开始

**预计时间**: Week 2-3

### 交付物

| 项目 | 状态 | 说明 |
|------|------|------|
| ls | ⏳ | 含分页测试 |
| mb | ⏳ | 创建桶 |
| rb | ⏳ | 删除桶 |
| cat | ⏳ | 输出内容 |
| head | ⏳ | 显示头部 |
| stat | ⏳ | 对象元数据 |
| 输出格式化 | ⏳ | human + JSON |
| Golden test | ⏳ | 快照测试 |

### 验收标准

- 每个命令至少 2 个退出码测试
- Golden test 覆盖所有 JSON 输出
- 分页合并测试（模拟 3 页）

---

## 阶段 3: 传输命令

**目标**: 实现对象传输相关命令

**状态**: ⏳ 待开始

**预计时间**: Week 4-6

### 交付物

| 项目 | 状态 | 说明 |
|------|------|------|
| cp | ⏳ | 本地↔S3, S3↔S3 |
| mv | ⏳ | 移动对象 |
| rm | ⏳ | 删除对象 |
| pipe | ⏳ | 标准输入上传 |
| Multipart | ⏳ | 含断点续传 |
| 进度条 | ⏳ | indicatif |
| 重试机制 | ⏳ | 指数退避 |

### 验收标准

- 大文件测试（>100MB）
- 断点续传测试（中断后继续）
- 重试测试（模拟 503）
- `--abort` 清理测试

---

## 阶段 4: 高级命令

**目标**: 实现高级操作命令

**状态**: ⏳ 待开始

**预计时间**: Week 7-8

### 交付物

| 项目 | 状态 | 说明 |
|------|------|------|
| find | ⏳ | 含过滤条件 |
| diff | ⏳ | 差异比较 |
| mirror | ⏳ | 增量同步 |
| tree | ⏳ | 树形显示 |
| share | ⏳ | PreSigned URL |

### 验收标准

- mirror 增量测试（只同步变化）
- find 过滤测试（name/size/time）

---

## 阶段 5: 可选命令

**目标**: 实现能力依赖的可选命令

**状态**: ⏳ 待开始

**预计时间**: Week 9-10

### 交付物

| 项目 | 状态 | 说明 |
|------|------|------|
| 能力检测 | ⏳ | capability.rs |
| version | ⏳ | 版本控制 |
| retention | ⏳ | 保留策略 |
| tag | ⏳ | 标签管理 |
| watch | ⏳ | 事件监听 |
| sql | ⏳ | S3 Select |

### 验收标准

- 每个命令在不支持时返回 EXIT_UNSUPPORTED_FEATURE (7)
- `--force` 绕过检测测试

---

## 阶段 6: 发布

**目标**: 完成发布准备工作

**状态**: ⏳ 待开始

**预计时间**: Week 11-12

### 交付物

| 项目 | 状态 | 说明 |
|------|------|------|
| 多平台构建 | ⏳ | Linux/macOS/Windows |
| Shell 补全 | ⏳ | bash/zsh/fish/powershell |
| README | ⏳ | 完整文档 |
| CHANGELOG | ⏳ | 变更日志 |
| Release CI | ✅ 完成 | 阶段 0 已完成 |

### 验收标准

- 所有平台二进制可用
- 补全脚本工作正常
- README 完整

---

## 风险跟踪

| 风险 | 状态 | 缓解措施 |
|------|------|----------|
| aws-sdk-s3 API 变化 | 监控中 | 通过 trait 抽象隔离 |
| S3 兼容性差异 | 监控中 | 能力分级 + 兼容矩阵 |
| 大文件传输性能 | 待验证 | 流式传输 + 分片 |

---

## 变更日志

| 日期 | 变更 |
|------|------|
| 2026-01-13 | 创建实施计划，开始阶段 0 |


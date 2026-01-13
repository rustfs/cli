# rc - Rust S3 CLI Client

[![CI](https://github.com/rustfs/rc/actions/workflows/ci.yml/badge.svg)](https://github.com/rustfs/rc/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)

一个用 Rust 编写的 S3 兼容命令行客户端，灵感来自 [minio/mc](https://github.com/minio/mc)。

## 特性

- 🚀 **高性能** - 使用 Rust 编写，支持异步并发操作
- 🔧 **S3 兼容** - 支持 RustFS、MinIO、AWS S3 及其他 S3 兼容服务
- 📦 **多平台** - 支持 Linux、macOS、Windows
- 🎨 **友好输出** - 支持人类可读和 JSON 格式输出
- 🔒 **安全** - 凭证安全存储，日志不泄露敏感信息

## 安装

### 二进制下载

从 [Releases](https://github.com/rustfs/rc/releases) 页面下载适合您平台的二进制文件。

### Homebrew (macOS/Linux)

```bash
brew install rustfs/tap/rc
```

### Cargo

```bash
cargo install rc
```

### 从源码构建

```bash
git clone https://github.com/rustfs/rc.git
cd rc
cargo build --release
```

## 快速开始

### 配置别名

```bash
# 添加 MinIO 服务
rc alias set minio http://localhost:9000 minioadmin minioadmin

# 添加 AWS S3
rc alias set s3 https://s3.amazonaws.com AKIAIOSFODNN7EXAMPLE wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY

# 列出所有别名
rc alias list
```

### 基础操作

```bash
# 列出桶
rc ls minio/

# 创建桶
rc mb minio/my-bucket

# 上传文件
rc cp ./file.txt minio/my-bucket/

# 下载文件
rc cp minio/my-bucket/file.txt ./

# 查看对象信息
rc stat minio/my-bucket/file.txt

# 删除对象
rc rm minio/my-bucket/file.txt

# 删除桶
rc rb minio/my-bucket
```

### 高级操作

```bash
# 递归复制目录
rc cp -r ./local-dir/ minio/bucket/remote-dir/

# 同步目录
rc mirror ./local-dir minio/bucket/remote-dir

# 查找对象
rc find minio/bucket --name "*.txt" --newer-than 1d

# 生成下载链接
rc share download minio/bucket/file.txt --expire 24h

# 监听事件
rc watch minio/bucket
```

## 命令概览

| 命令 | 说明 |
|------|------|
| `alias` | 管理存储服务别名 |
| `ls` | 列出桶或对象 |
| `mb` | 创建桶 |
| `rb` | 删除桶 |
| `cp` | 复制对象 |
| `mv` | 移动对象 |
| `rm` | 删除对象 |
| `cat` | 输出对象内容 |
| `head` | 显示对象头部 |
| `stat` | 显示对象元数据 |
| `find` | 查找对象 |
| `diff` | 比较两个位置 |
| `mirror` | 镜像同步 |
| `tree` | 树形显示 |
| `share` | 生成分享链接 |
| `pipe` | 从标准输入上传 |

### 可选命令（需要后端支持）

| 命令 | 说明 |
|------|------|
| `version` | 管理桶版本控制 |
| `retention` | 管理对象保留策略 |
| `tag` | 管理对象标签 |
| `watch` | 监听对象事件 |
| `sql` | 执行 S3 Select 查询 |

## 输出格式

### 人类可读（默认）

```bash
rc ls minio/bucket
[2024-01-15 10:30:00]     0B dir/
[2024-01-15 10:30:00] 1.2MiB file.txt
```

### JSON 格式

```bash
rc ls minio/bucket --json
```

```json
{
  "items": [
    {"key": "dir/", "is_dir": true},
    {"key": "file.txt", "size_bytes": 1258291, "size_human": "1.2 MiB", "is_dir": false}
  ],
  "truncated": false
}
```

## 配置文件

配置文件位于 `~/.config/rc/config.toml`：

```toml
schema_version = 1

[defaults]
output = "human"
color = "auto"
progress = true

[[aliases]]
name = "minio"
endpoint = "http://localhost:9000"
access_key = "minioadmin"
secret_key = "minioadmin"
region = "us-east-1"
```

## 退出码

| 码 | 说明 |
|----|------|
| 0 | 成功 |
| 1 | 一般错误 |
| 2 | 参数/路径错误 |
| 3 | 网络错误（可重试） |
| 4 | 认证/权限错误 |
| 5 | 资源不存在 |
| 6 | 冲突/前置条件失败 |
| 7 | 功能不支持 |
| 130 | 被中断 (Ctrl+C) |

## 兼容性

### 支持的后端

| 后端 | 级别 | 说明 |
|------|------|------|
| RustFS | Tier 1 | 完全支持 |
| MinIO | Tier 2 | 完全支持 |
| AWS S3 | Tier 3 | 尽力支持 |
| 其他 S3 兼容 | Best Effort | 不保证 |

### 最低 Rust 版本

- Rust 1.75 或更高

## 开发

### 构建

```bash
cargo build --workspace
```

### 测试

```bash
# 单元测试
cargo test --workspace

# 集成测试（需要 MinIO）
docker compose -f docker/docker-compose.yml up -d
cargo test --workspace --features integration
docker compose -f docker/docker-compose.yml down
```

### 格式检查

```bash
cargo fmt --all --check
cargo clippy --workspace -- -D warnings
```

## 贡献

欢迎贡献！请阅读 [AGENTS.md](AGENTS.md) 了解开发规范。

## 许可证

本项目采用 MIT 或 Apache-2.0 双许可证。详见 [LICENSE-MIT](LICENSE-MIT) 和 [LICENSE-APACHE](LICENSE-APACHE)。

## 致谢

- [minio/mc](https://github.com/minio/mc) - 设计灵感来源
- [aws-sdk-s3](https://crates.io/crates/aws-sdk-s3) - S3 SDK


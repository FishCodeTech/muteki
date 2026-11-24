---
title: 使用 LayerFS 进行本地开发
description: "使用 Native Shell 仓库中的 Linux LayerFS 流程保存 Cargo 构建产物、分叉工作区并验证快照复用。"
order: 61.5
group: tools
---

Native Shell 仓库提供了一套本地 LayerFS 流程，用来验证构建工作区的复用。它导入已提交的源码，在 Linux
Sandbox 中执行 Cargo，保存生成的文件，再从保存状态分叉工作区、重新运行测试。源码编辑和 Git 提交仍在现有的
NSH checkout 中进行。

> [!IMPORTANT]
> 这套流程需要 Docker Engine 和 Linux FUSE。Docker 运行控制进程与 Sandbox，FUSE 在 Sandbox 内挂载工作区。
> 在 macOS 上，开发命令通过 OrbStack 等本地 Docker 环境准备这套 Linux 部署。

## 快照保存什么

LayerFS 保存的是文件系统树：源码、Git 元数据、被 Git 忽略的文件，以及在工作区中生成的构建产物。分叉从保存的树
开始，记录各自的修改，让多个任务可以从同一个已构建状态出发，并保持修改相互隔离。

NSH 的本地验证在第一个分支上运行 `nsh-package` 的测试，保存 `target/` 和一个被 Git 忽略的探针文件。第二个
分支从这个提交出发，检查文件是否保留，再离线运行测试。第三个分支从原始源码状态出发，验证隔离性。

| 阶段 | 执行内容 |
| --- | --- |
| 初始化 | 导入 checkout 的 `HEAD` 提交导出和新建的 `.git` 目录 |
| 构建 | 分叉工作区，运行 `cargo +1.98.1 test --locked -p nsh-package` |
| 提交 | 将生成的 `target/` 和探针文件保存到 LayerFS 历史 |
| 复用 | 从已保存分支分叉，挂载后加上 `--offline` 重新运行测试 |
| 验证 | 读取提交历史，检查原始源码分支保持干净 |

每次调用都会创建新的 Store，执行上述完整流程。快照在同一次调用的分支之间复用。评估时要测量的是获得另一个可用、
已测试工作区的完整成本，包括挂载、提交和卸载的耗时。

## 准备开发环境

准备 [NSH 仓库](https://github.com/longbridge/nsh) 的 checkout，以及：

- 本地 Linux Docker 引擎，其 Store 卷位于 ext4、XFS 或 Btrfs 文件系统；可运行在 Linux 上，或 macOS 的 OrbStack 等环境中。
- 提供 API 1.54 或更新版本的 Docker Engine；本地验证使用 Engine 29.7.2。
- `/dev/fuse`，并允许 Docker 将该设备和 `CAP_SYS_ADMIN` 提供给 Sandbox。
- Git 和 Python 3.12 或更新版本；在 Linux 上还需要 rustup 和 C/C++ 编译器。
- 可获取固定版本 LayerFS 源码、Cargo 依赖和运行镜像的网络连接。

这套流程构建 LayerFS **1.0.0**，固定源码提交为
`2aaf2552ce21154928b9f9c83582e41a637f5910`。运行镜像为 Linux 进程提供 Rust 1.98.1、Node 和 clang。
[上游 SDK 指南](https://github.com/Ephemeral-AI-Lab/layerfs/blob/v1.0.0/sdk/js/README.md) 介绍了 Store 和
Sandbox 的生命周期。

在 NSH checkout 根目录运行：

```sh
./scripts/layerfs prepare
./scripts/layerfs smoke
./scripts/layerfs status
```

入口可直接执行，会自动选择 Python 3.12 或更新版本，也支持 macOS 上的 Homebrew 安装，并从当前 Docker context
发现 socket。需要指定本地 socket 时，使用 `--docker-socket PATH`。

`prepare` 检查 Docker API，安装 Rust 1.85.1，构建固定版本的 daemon、Node addon 和运行镜像。在 macOS 上，
这些准备工作在临时 Linux 构建容器中执行。它复用通常的 Cargo registry 和 `target/`，并沿用已有的
`CARGO_TARGET_DIR` 设置。镜像的精确 ID 和 daemon 路径记录在 `.nsh/layerfs/deployment.json`。
`status` 检查保存的构建产物和镜像，显示最近一次验证结果。

`smoke` 从 checkout 的**已提交 `HEAD`** 创建 `.nsh/layerfs/nsh-<id>/input`，再将其导入新的 Store。
要包含源码修改，先将修改提交到自己的主题分支。生成的输入具有独立的新 Git 仓库，构建产物随后在 Sandbox 中生成。

控制进程打开具名本地 Docker 卷中的 Store。Sandbox daemon 打开同一个 Store，挂载分支对应的文件系统视图；
Cargo 在这个挂载的工作区中执行。原有 checkout 继续用于编辑和提交源码。

## 查看结果

命令会打印记录目录和 Store 卷名。打开生成的 `result.json`，检查 `"ok": true` 以及各阶段的耗时。同目录中的
`settings.json` 标识源码提交和镜像，`trace.jsonl` 记录 SDK 调用。

本地验证在构建和复用阶段分别运行了 22 项包测试。首次构建测试包含依赖下载；离线阶段使用保存的 `target/` 和同一个
Sandbox 的 Cargo registry。评估收益时，请使用自己的工作负载，将完整流程与普通 Cargo 构建进行比较。

复用构建状态时，操作系统、架构、工具链和依赖需要匹配。本指南的工作负载是 Linux 上的 `nsh-package` 测试集；
平台 UI 检查在各自的平台环境中运行（[测试](./testing-and-trace.md)）。

## 保留有用的工作并管理 Store

源码修改保存在 Git 中，通过 pull request 提交。这套流程使用 LayerFS 的 **Disposable** Store 配置保存可重建的
工作区历史。记录测量结果时，将需要保留的记录另外保存。

生成的输入仓库在首次提交时关闭自动 Git 维护，使 LayerFS 导入期间的对象数据库保持稳定。验证流程中的 Git 状态
检查使用 `--no-optional-locks`，读取状态时保留原始索引。编写自己的脚本时，应在导入或提交文件树之前等待后台写入
进程结束。

成功卸载的 Sandbox 会在 LayerFS 的空闲超时后停止。每次验证都会保留自己的 Store 卷；根据记录中的标识管理对应的
卷和容器。操作失败时，保留记录的状态用于排查。[维护者参考](../../../docs/dev/layerfs.md) 介绍了脚本、记录和资源
清理方式。

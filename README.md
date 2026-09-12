# dvup

[![CI](https://github.com/ZyphrZero/dvup/actions/workflows/ci.yml/badge.svg)](https://github.com/ZyphrZero/dvup/actions/workflows/ci.yml)

一个零配置、跨平台的工具链更新器。开箱即用，自带 TUI，能处理运行中的进程、文件占用和包管理器资源锁，支持 Windows、macOS 和 Linux。

## 特性

- **零配置启动**：内置常用开发工具的更新规则，安装后直接使用，无需编写配置文件
- **交互式 TUI**：浏览已安装/最新版本、批量勾选并行更新、内置语法高亮的 TOML 编辑器（支持 Vim 模式）
- **锁感知更新**：等待或安全终止占用文件的进程，自动重试 `EBUSY`、sharing violation 等错误，更新器自身被占用时由后台 worker 完成替换
- **并行更新**：独立工具并行执行，共享同一安装目录的工具（如多个 npm 全局包）自动串行排队
- **安装冲突诊断**：`doctor` 按真实 PATH 顺序找出重复安装、被遮蔽的旧版本和路径冲突
- **GitHub Release 监控**：跟踪任意仓库的 Release，用语义选择器精确匹配资产，支持直接安装
- **对称的卸载**：包管理器包、自定义命令和内置工具都能按各自声明的反安装命令移除，同样锁感知
- **自定义更新命令**：一条命令添加任意工具的更新方式，也可在 TUI 向导中通过官方 Registry 验证后添加
- **中英双语界面**：TUI 内按 `L` 随时切换

## 安装

```console
cargo install dvup --locked
```

或从 [GitHub Releases](https://github.com/ZyphrZero/dvup/releases) 下载 Windows、Linux、macOS 的 x86_64/ARM64 归档（附 `SHA256SUMS`）。

安装 dvup 后，可以用它更新自己：`dvup self-update`。

## 快速开始

直接运行进入交互界面：

```console
dvup
```

界面列出所有工具的安装状态、已安装版本和最新版本（后台异步探测，不阻塞界面）。方向键选择，`Enter` 更新，`Space` 多选后并行更新，已是最新版的工具会自动跳过。

也可以完全在命令行完成日常工作：

```console
dvup list                 # 查看本机可更新的工具
dvup update               # 更新所有已安装的工具
dvup update rustup        # 只更新一个工具
dvup doctor               # 诊断 PATH 中的安装冲突
```

## 命令行参考

| 命令 | 说明 |
| --- | --- |
| `dvup` / `dvup tui` | 启动交互式 TUI |
| `dvup list` | 列出内置和自定义工具及安装状态 |
| `dvup update [tool] [args...]` | 更新全部或指定工具，参数追加到工具命令 |
| `dvup update <tool> --to <version>` | 将工具更新或降级到精确版本（需工具支持） |
| `dvup add <name> <command...>` | 添加用户级自定义更新命令 |
| `dvup remove <name>` | 删除自定义命令声明 |
| `dvup uninstall <tool>` | 卸载工具（包管理器包走官方反安装命令） |
| `dvup uninstall <tool> --purge` | 卸载后同时删除该工具的用户声明 |
| `dvup init` | 创建全局用户配置文件 `dvup_custom.toml` |
| `dvup doctor [tool]` | 诊断重复安装和版本冲突 |
| `dvup self-update [--force]` | 从 crates.io 更新 dvup 自身 |
| `dvup jobs [id] [--log]` | 查看任务列表（含前台执行）或单个任务的日志 |
| `dvup run -- <command...>` | 以锁感知方式运行任意命令 |
| `dvup <file.toml>` | 直接在 TUI 中打开并编辑该配置文件 |

常用选项：`--config <path>` 指定用户清单（替代全局配置），`--state-dir` / `DVUP_STATE_DIR` 隔离状态目录，`--background auto|always|never` 控制后台执行，`--terminate-locking-processes` 将等待规则转为安全终止。

## 内置工具

| 名称 | 更新方式 | 平台 | `dvup uninstall` |
| --- | --- | --- | --- |
| `dvup` | `cargo install dvup --locked`（后台替换自身） | 全平台 | `cargo uninstall dvup` |
| `bun` | 官方安装器（Windows: `install.ps1`；Unix: Bash 安装器） | 全平台 | 未声明，需自行配置 |
| `brew` | `brew update`（拉取 Homebrew 自身及软件源） | macOS、Linux | `brew uninstall brew` |
| `deno` | `deno upgrade` | 全平台 | 未声明，需自行配置 |
| `mise` | `mise self-update` | 全平台 | `mise implode --yes` |
| `pixi` | `pixi self-update` | 全平台 | 未声明，需自行配置 |
| `rustup` | `rustup update` | 全平台 | `rustup self uninstall -y` |
| `scoop` | `scoop update` | Windows | `scoop uninstall scoop` |
| `uv` | Astral 官方安装器 | 全平台 | 未声明，需自行配置 |

`dvup update` 会跳过未安装或当前平台不支持的工具，单项失败不影响其他工具，结束后统一汇总。

## 卸载工具

`dvup uninstall <tool>` 用工具自己的方式把它从本机移除，和更新一样锁感知：工具正在运行时按进程策略等待或终止，绝不在占用状态下硬删文件。

```console
dvup uninstall ripgrep           # brew uninstall ripgrep（由 manager 模板生成）
dvup uninstall codegraph         # npm uninstall --global @colbymchenry/codegraph
dvup uninstall rustup            # rustup self uninstall -y
dvup uninstall release-tool --purge   # 卸载后同时删除这条用户声明
```

反安装命令的来源分三种：

| 工具类型 | 反安装命令来源 |
| --- | --- |
| `type = "package"`（`dvup add` 的包管理器路径、TUI 添加） | 由 manager 模板本地确定：`brew uninstall`、`npm uninstall --global`、`pnpm remove --global`、`bun remove --global`、`cargo uninstall`、`pipx uninstall`、`uv tool uninstall` |
| `type = "custom"` | 由 `uninstall = [...]` 字段显式声明；没写字段时，从更新命令推导单包安装的反安装命令（`npm install -g <包>`、`pnpm add -g`、`bun add -g`、`brew install`、`cargo install`、`pipx install`、`uv tool install`），其余形态拒绝卸载并提示如何补充 |
| 内置预置 | 仅在内置清单里确实存在确定性反安装命令时才提供：`dvup`、`rustup`、`brew`、`scoop`、`mise` |

推导只认**明确的单包全局安装**：`npm install <包>`（本地安装）、一次装多个包、路径安装都不会被猜成全局卸载。像 `bun`、`uv`、`deno`、`pixi` 这类只能靠删除安装目录、改 shell 配置或 PATH 才能移除的工具，dvup 也不会猜命令——需要的话在声明里写清 `uninstall`。

TUI 里按 `u` 时，确认框会显示将要执行的反安装命令；如果这条命令是推导来的，还会额外标注，提醒你它并不在声明文件里（按 `e` 编辑声明即可写入或用 `uninstall` 覆盖）。

卸载默认**保留配置声明**，工具随后显示为 `missing`，随时可以重新安装或继续更新；只有显式加 `--purge` 才会连声明一起删掉（TUI 里对应 `d` 键，只删声明、不执行反安装命令）。

## 添加自定义工具

最简单的方式，无需编辑 TOML：

```console
dvup add claude claude update
dvup update claude
```

更新 npm/pnpm/Bun 全局包或 Homebrew formula/cask，就是为目标包各保存一条命令：

```console
dvup add ripgrep brew upgrade ripgrep
dvup add zed brew upgrade --cask zed
dvup add codegraph npm install --global @colbymchenry/codegraph@latest
```

保存前 dvup 会实际运行只读探针（默认 `<name> --version`）确认能取到版本号；更新命令本身不会被预执行。替换同名命令需 `dvup add --force`。

命令按 argv 安全拆分，不经 shell 拼接，支持绝对路径（如 `/opt/homebrew/bin/brew`）。通过 npm/pnpm 添加的命令自动共享 `node-global` 资源组，Bun 全局包用独立的 `bun-global` 资源组（和 npm 的全局目录不是一处），Homebrew 包共享 `homebrew` 资源组——共享同一安装目录的任务会自动排队，不同资源组之间仍然并行。

在 TUI 的 Tools 页按 `c` 还有两条引导路径：**从包管理器添加**（Homebrew/npm/pnpm/Bun/Cargo/pipx/uv，经官方 Registry 验证，自动发现可执行文件）和 **AI 分析**（可选，仅提取包管理器和包名，验证流程与手动完全一致）。添加自定义命令的向导里，能识别出包管理器安装时还会预填一条卸载命令（可改可清空），留空且无法识别时表示该命令不支持卸载。

## GitHub Release 监控

在 TUI 的 Tools 页按 `Tab` 切换到 GitHub 仓库视图，按 `c` 添加。输入 `owner/repo` 后，dvup 会调用 GitHub API 验证仓库并展示最新 Release 的真实资产列表（已过滤源码包、签名、SBOM 和不兼容平台的资产），由你明确选择一个文件，随后生成语义选择器（product/os/arch/format/变体）并回验唯一匹配——以后每个新 Release 都必须精确命中一次，不会"猜"文件。

- 普通文件、ZIP、TAR.GZ 安装到 dvup 用户目录的 `github-tools/<名称>` 下
- DMG 指定 `.app` 名称，安装到 `/Applications`
- `update_policy = "automatic"` 时，新 Release 自动进入安装队列

```toml
[github.monitors.ripgrep]
repository = "BurntSushi/ripgrep"
asset = { product = "ripgrep", os = "macos", arch = "aarch64", format = "tar_gz" }
install = { type = "user_directory" }
```

## 运行中的进程与后台任务

更新时如果目标工具正在运行（比如正在使用 Claude Code 时更新 `claude`），dvup 不会粗暴失败：

- 默认策略 `WAIT`：任务转入后台，等待进程退出后自动继续
- `TERMINATE` 策略（TUI 按 `Shift+Tab` 切换，或 CLI 加 `--terminate-locking-processes`）：由后台 worker 精确终止匹配的进程后立即更新
- 终止始终带身份校验（PID + 进程名 + 启动时间），只终止命令行能确认属于目标工具的进程，绝不按名称批量杀进程
- 遇到 `EBUSY`、文件占用、npm 锁相关 `EPERM` 时自动重试

**每一次更新/卸载都是一条持久任务记录**：前台直接跑完的也会记下 `运行中 → 成功/失败` 和完整命令输出，只有撞到占用需要延后的才交给后台 worker 继续（同一个任务 id 从"运行中"变成"等待执行"，完成后变"成功"，全程可追踪）。所以任务列表是执行历史，而不是只显示排队项：

命令**退出码为 0、但输出里仍报告占用或权限错误**时（典型例子：`rustup update` 的 self-update 因为 `rustup.exe` 被占用而失败，但 rustup 仍返回成功），状态会标成 `succeeded (warned)`／`成功（有告警）`，日志里保留原始错误——不会出现"列表说成功、展开却全是 error"的落差。识别同时匹配英文标记与本地化消息里的 `os error 5/32/33`（Windows）或 `os error 13/16/26`（Unix）等系统错误码，中文系统也能判出来。

```console
dvup jobs                 # 任务列表（含前台执行的更新/卸载）
dvup jobs <job-id> --log  # 单个任务的输出日志
```

TUI 的 **Jobs** 页同等内容：按 Enter 展开日志，在结果面板里鼠标拖选即可选中，按 `y` 把选区复制到剪贴板，Esc 取消选区。状态目录位置：Windows `%LOCALAPPDATA%\dvup\data`，Linux `~/.local/share/dvup`，macOS `~/Library/Application Support/dev.dvup`。

## 诊断安装冲突

`doctor` 按 PATH 实际顺序检查所有工具，找出重复安装、被遮蔽的旧版本和工具/更新器之间的路径冲突：

```console
dvup doctor             # 检查所有配置工具
dvup doctor rustup      # 只检查一个工具
```

```text
[WARN] uv
  command: uv
  active: C:\manager-a\bin\uv.exe  [PATH]  version 0.11.28
  shadowed: C:\Users\me\.local\bin\uv.exe  [user-local]  version 0.10.0
  conflict: PATH candidates report different versions
  fix: remove the stale installation from PATH or move the intended one first
```

同一工具由包管理器产生的多个启动器（如 npm/Scoop 生成的 `.ps1`/`.cmd`/无扩展名文件）、rustup 代理与实际工具链、cargo build 临时目录等托管路径会被正确归并，不会误报。该命令完全只读；发现冲突时退出码为 `1`，可直接用于 CI。

## TUI

顶部标签页：**Tools**（命令工具 / GitHub 仓库两个视图）、**Activity**（执行历史，含 stdout/stderr）、**Jobs**（后台任务）、**Doctor**（诊断）、**Settings**。

| 按键 | 操作 |
| --- | --- |
| `↑` `↓` / `j` `k` | 移动选择、滚动 |
| `Space` | 勾选工具 / 切换选项 |
| `a` | 全选/取消已安装工具 |
| `Enter` | 更新、展开详情或确认 |
| `Tab` | 切换命令工具 / GitHub 仓库视图 |
| `c` | 添加命令或 GitHub 监控 |
| `e` | 编辑当前工具 |
| `d` | 删除选中的自定义项 |
| `u` | 卸载选中的已安装工具（需确认） |
| `t` / `o` | 内置 TOML 编辑器 / 系统编辑器打开配置 |
| `v` | 指定目标版本（需工具支持） |
| `r` | 重载配置并刷新 |
| `Shift+Tab` | 切换 `WAIT` / `TERMINATE` 进程策略 |
| `L` | 切换中英文界面 |
| `←` `→` | 切换标签页 |
| `Ctrl+C` | 按两次退出 |

内置 TOML 编辑器提供 Taplo 语法高亮、多行粘贴、撤销/重做、块注释（`Ctrl+/`），按 `F2` 切换 Vim 模式，`Ctrl+S` 校验并保存（无效内容不会覆盖磁盘文件）。也支持鼠标：点击切换标签页、勾选、展开详情，滚轮滚动。

## 网络与设置

设置保存在状态目录的 `settings.toml`，可在 TUI 的 Settings 页直接修改，包括界面语言、启动诊断、GitHub API Key（系统凭据管理器加密存储，明文不出现在配置和日志中）、可选的 AI 辅助配置（OpenAI 兼容接口）。

代理有三种互斥模式：

| 模式 | 行为 |
| --- | --- |
| `environment`（默认） | 继承环境变量 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY` |
| `explicit` | 只使用 Settings 中填写的代理地址，忽略环境变量 |
| `direct` | 显式关闭代理，并从子进程环境中移除所有代理变量 |

```toml
[network]
proxy_mode = "environment"
metadata_timeout_secs = 10
release_asset_setup_timeout_secs = 30
release_asset_body_timeout_secs = 300
```

服务器上最简单的方式是保持 `environment` 模式，在 systemd unit 的 `Environment=` 或 Docker 的 `-e` 中注入标准代理变量。三种模式都不会在代理失败时静默回退直连。

## 配置文件

普通用法不需要配置文件——内置预置始终生效，自定义内容保存在用户数据目录的全局 `dvup_custom.toml`（`dvup init` 创建，或直接在 TUI 中编辑）。文件只有两个顶层分区，schema 严格校验，未知字段直接报错：

```toml
# 包管理器命令：更新命令、反安装命令、最新版本来源、资源组等由 manager 模板本地确定
[commands.codegraph]
type = "package"
manager = "npm"
package = "@colbymchenry/codegraph"
executable = "codegraph"

# 自定义命令：argv 数组，不经 shell；可选显式声明最新版本来源和卸载命令
[commands.deno]
type = "custom"
update = ["deno", "upgrade"]
probe = ["deno", "--version"]
latest = { provider = "github_release", repository = "denoland/deno" }

[commands.mise]
type = "custom"
update = ["mise", "self-update"]
probe = ["mise", "--version"]
uninstall = ["mise", "implode", "--yes"]

# GitHub Release 监控
[github.monitors.ripgrep]
repository = "BurntSushi/ripgrep"
asset = { product = "ripgrep", os = "macos", arch = "aarch64", format = "tar_gz" }
install = { type = "user_directory" }
```

`latest` 支持 `npm`、`pypi`、`crates_io`、`github_release`、`github_tag` 五种来源。完整示例见 [configs/dvup.user.example.toml](configs/dvup.user.example.toml)。写入采用临时文件 + 原子替换，失败时保留原文件。

## 构建

```console
cargo build --release
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

CI 在 ubuntu、macos 和 windows 三个平台上执行格式检查、Clippy、全部测试和 release 构建。

## 许可证

[MIT](LICENSE)

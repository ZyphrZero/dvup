# dvup

一个零配置、跨平台、能处理运行中进程、文件占用和包管理器资源锁的工具链更新器，支持 Windows、macOS 和 Linux。

## 开始使用

直接运行即可进入交互界面，不需要准备配置文件：

```console
dvup
```

界面会列出所有内置和自定义工具、安装状态、当前版本、更新命令（或简短友好名称）与最近一次结果。版本通过后台只读探测异步回填，不会阻塞 TUI；无法识别时显示 `—`。用方向键选择工具，按 `Enter` 确认后更新当前工具；也可以用 `Space` 选择多项后一起并行更新。

| 按键 | 操作 |
| --- | --- |
| `↑` / `↓`、`j` / `k` | 移动选择或滚动日志 |
| `Space` | 选择或取消一个工具；在 Settings 页切换选项 |
| `a` | 选择或取消全部已安装工具 |
| `Enter` | 确认更新；在 Jobs 页展开或收起任务结果；在 Doctor 页首次启动扫描或展开结果；在 Settings 页切换选项 |
| 鼠标左键 | 点击顶部页签切换视图；在 Tools 页勾选工具；在 Activity 页展开执行输出；在 Jobs 页展开任务结果；在 Doctor 页展开诊断详情；在 Settings 页切换选项 |
| 鼠标移动 | Tools/Jobs/Doctor/Settings 行焦点跟随鼠标；页签和 Activity 执行标题显示悬停高亮，不自动触发操作 |
| 鼠标滚轮 | 每格滚动一行；Tools/Jobs/Doctor 列表滚动后焦点保持在鼠标所在屏幕行；详情区、Activity 和 TOML 编辑器逐行滚动内容 |
| `PgUp` / `PgDn` | 在 Jobs 页滚动已展开的任务结果 |
| `t` | 在 Tools 页打开当前生效配置的 TOML 编辑视图 |
| `c` | 添加自定义更新命令 |
| `e` | 编辑或重命名选中的自定义命令 |
| `d` | 删除选中的自定义命令（内置工具不能删除） |
| `→` | 切换到下一个页面 |
| `←` | 切换到上一个页面 |
| `Shift+Tab` | 在 `WAIT` 与 `TERMINATE` 进程策略间切换 |
| `L` | 在中文与英文界面间切换 |
| `r` | 刷新当前视图；在 Doctor 页启动或重新执行诊断扫描 |
| `q` | 无运行中操作时退出；有操作尚未结束时防止误退 |
| `Ctrl+C` | 第一次显示提醒，连续第二次退出；TOML 编辑视图中改为复制选区，不触发退出 |

更新完成后，Tools 页会显示每项的 `updated`、`queued` 或 `failed` 状态和耗时；Bun、uv 等内置工具在日常界面中使用简短的友好名称，不展示冗长的底层安装命令。TUI 顶部状态栏显示当前本地日期时间。Activity 页保留退出码、stdout/stderr 和整批汇总，每条记录都显示产生时的本地日期时间；每次执行默认折叠，点击带 `▸` 的执行标题可原地展开输出，并使用青色表示启动、绿色表示成功、黄色表示排队或人工处理提示、红色表示失败、蓝色表示任务元数据。Jobs 页显示任务更新时间，点击对应任务（或按 `Enter`）会在当前页面展开结果，再次点击即可收起，不会跳转到 Activity。新写入的持久化 Job 日志会为每行 worker 状态和命令输出添加日期时间；`dvup jobs` 列表和详情也会显示任务时间。外部命令的 ANSI 控制序列、回车覆盖和纯进度条会在渲染前清理，避免动态终端输出破坏 TUI 布局；普通日志和失败正文仍会完整保留。失败诊断和 Jobs 任务详情仍保留实际命令，方便排错。也可以显式使用 `dvup tui` 启动界面。

Doctor 页提供与 `dvup doctor` 相同的安装冲突诊断。默认情况下，进入 TUI 或切换到该页都不会自动执行诊断；从未扫描时会明确显示提示，按 `Enter` 开始首次扫描，之后按 `r`/`R` 主动重新扫描。扫描在后台运行，不会阻塞界面，扫描期间再次按 `Enter` 或 `R` 不会创建重复任务。表格显示每个工具的状态、当前生效路径、版本、安装数量和更新器。已有结果时，点击工具行或按 `Enter` 会在当前页面展开 active/shadowed 完整路径、安装来源、版本差异和处理建议，再次操作即可收起；鼠标滚轮或 `PgUp`/`PgDn` 可以滚动长详情。标题会显示最近检查的本地日期时间；工具更新、配置增删改或后台 Job 完成不会隐式覆盖结果。

Settings（设置）页目前提供两个默认关闭的选项：“进入 TUI 时自动运行 Doctor 诊断”和“隐藏不支持或未安装的工具”。使用上下方向键或鼠标移动焦点，再用鼠标点击、`Space` 或 `Enter` 切换。修改会立即保存；自动诊断选项不会在当前会话中突然启动扫描，而是从下次进入 TUI 起在后台自动运行一次只读诊断。这个选项只影响 TUI 启动，不会让“切换到 Doctor 页”重新成为扫描触发条件；Doctor 页的 `Enter` 和 `R` 手动扫描始终可用。工具过滤选项只在内存中根据已有状态重建 Tools 可见行，不会重新扫描 PATH、启动 PowerShell 或重复执行版本探测，因此会立即响应；它只隐藏 `unsupported` 和 `missing`，目标已安装但仅缺少更新器的工具仍会保留，关闭后按原顺序恢复完整列表。设置保存在状态目录的 `settings.toml` 中，使用 `--state-dir` 时会随指定状态目录隔离。

TUI 使用统一的暗色语义配色、圆角弱边框、选中行底色和滚动条。确认更新、添加命令、确认保存与删除窗口会显示为居中的独立面板，并压暗底层视图；子窗口存在期间，键盘和鼠标输入只交给当前子窗口，不能切换页签、勾选工具、展开 Activity/Jobs、滚动底层内容、切换进程策略或退出程序。普通确认框和单行表单可用 `Esc` 或 `Ctrl+C` 关闭/返回，确认操作仍使用 `Enter`/`y`；TOML 编辑视图中的 `Ctrl+C` 专用于复制，不会关闭窗口或退出 dvup。

在 Tools 页按 `t` 会打开全屏 TOML 编辑视图。使用 `--config` 时编辑该显式文件；否则始终编辑用户数据目录中的全局 `dvup_custom.toml`，不会扫描或创建当前项目配置。目标文件尚不存在时，编辑器会用当前聚焦工具生成一份可通过校验的初始配置，直到按 `Ctrl+S` 才真正创建文件。编辑器使用 Taplo 语法树高亮键名、字符串、数字、布尔值、日期时间、注释和错误标记；即使文件尚未写完或语法无效，也会保留并高亮全部原始内容。编辑器支持方向键、`Home`/`End`、`PgUp`/`PgDn` 移动光标，按住 `Shift` 扩展选区，鼠标点击定位、拖拽选择、滚轮滚动；`Ctrl+A` 全选，`Ctrl+C` 复制原始 TOML 选区，`Ctrl+V` 或终端原生多行粘贴会替换选区。`Ctrl+/` 会对当前行或选区覆盖的所有完整行统一增加或移除 `# `，并保留整行选区；`Ctrl+Z` 撤销，`Ctrl+Y` 或 `Ctrl+Shift+Z` 重做。输入批次、多行粘贴和整块注释各自只占一个撤销步骤，历史最多保留 100 步且总快照约束为 8 MiB。终端把粘贴拆成连续字符事件时，dvup 会合并为批量文本插入，避免逐字符解析和重绘。`Ctrl+S` 会先执行完整 TOML 与 dvup 配置校验，再原样保存注释和排版并刷新工具列表；无效内容不会覆盖磁盘文件。按 `Esc` 关闭编辑视图。

也可以把一个已经存在的 `.toml` 文件直接传给 dvup，跳过 Tools 页面并立即进入编辑器；Windows 文件关联或把 TOML 文件拖到 `dvup.exe` 上时使用的是同一入口：

```console
dvup path/to/dvup_custom.toml
```

直达模式只在启动时检查文件存在且扩展名为 `.toml`，不会要求内容已经是有效配置，因此可以直接打开语法损坏或缺少字段的文件进行修复。只有按 `Ctrl+S` 时才执行完整 dvup 配置校验；校验失败仍不会覆盖原文件。

TUI 默认使用英文，随时按 `L` 可切换为中文，再按一次切回英文。页签、表头、工具与任务状态、帮助栏、表单、确认框、进程策略以及 dvup 在界面内生成的运行摘要都会即时使用当前语言；已经写入 Activity 的历史记录保留产生时的语言，外部工具 stdout/stderr 和持久化 Job 日志始终保持原文，确保诊断内容不被翻译改写。在添加命令的文本输入框和 TOML 编辑视图中，`l`/`L` 仍作为普通字符输入；退出输入界面后即可继续用 `L` 切换语言。

在 TUI 中按 `c`，只需填写名称和命令。例如名称填写 `claude`，命令填写 `claude update`；也可以填写 `npm install -g package@latest`、`pnpm add -g package@latest` 或 `brew upgrade ripgrep`。填写后会先出现预览确认框，并明确提示“只保存，不执行”。保存完成后界面返回 Tools 页面并聚焦新工具；只有之后再次按 `Enter` 并确认更新，命令才会真正执行。

## 命令行用法

原有命令行模式完整保留，适合脚本和自动化：

```console
# 查看本机可更新的工具
dvup list

# 诊断 PATH 中的重复安装和版本冲突
dvup doctor
dvup doctor rustup

# 更新所有已经安装的工具
dvup update

# 只更新 dvup 自己；始终通过后台副本完成替换
dvup self-update

# 即使已经是当前版本也重新安装
dvup self-update --force

# 只更新一个工具
dvup update rustup

# 拉取 Homebrew 自身及软件源更新
dvup update brew

# 使用 Astral 官方安装器更新 uv
dvup update uv

# 给工具追加参数
dvup update scoop zedg

# 添加自己的更新命令（用户级、永久保存）
dvup add claude claude update
dvup update claude

# macOS/Linux：添加一个 Homebrew formula 更新
dvup add ripgrep brew upgrade ripgrep
```

不需要先创建配置文件。dvup 始终加载内置预置，并在全局 `dvup_custom.toml` 存在时应用用户自定义。

`dvup self-update` 从 crates.io 执行 `cargo install dvup --locked`。该任务始终由复制到状态目录的 detached worker 执行；在 TUI 中更新 `dvup` 时，任务会排队等待当前界面退出，然后替换 Cargo bin 目录中的原可执行文件。`dvup update dvup` 使用同一套机制。

## 诊断安装冲突

`doctor` 会按系统实际的 PATH 顺序检查所有已配置工具，找出同一命令的重复安装、被遮蔽的旧版本，以及工具与其更新器之间可能存在的路径冲突：

```console
# 检查所有配置工具
dvup doctor

# 只检查一个内置工具
dvup doctor rustup

# 使用指定配置
dvup doctor --config path/to/dvup_custom.toml
```

也可以直接启动 `dvup`，通过键盘 `←`/`→` 或鼠标点击进入顶部的 Doctor（诊断）页；TUI 使用相同的诊断逻辑，并以可展开表格显示结果。

报告中的 `active` 是当前实际优先解析到的安装，`shadowed` 是 PATH 后方的同名安装；每项同时显示识别到的安装来源和 `--version` 结果。Windows 中同一目录下由 npm、Scoop 等同时生成的 `.ps1`、`.cmd` 和无扩展名启动器属于同一套安装，不会被误报为多套。对于 npm、pnpm、Bun、Scoop 或 Homebrew 管理的工具，doctor 还会按需检查对应更新器，避免“目标工具来自一处、包管理器却来自另一处”的问题被遗漏。

例如：

```text
[WARN] uv
  command: uv
  active: C:\manager-a\bin\uv.exe  [PATH]  version 0.11.28
  shadowed: C:\Users\me\.local\bin\uv.exe  [user-local]  version 0.10.0
  conflict: PATH candidates report different versions
  fix: remove the stale installation from PATH or move the intended one first
```

该命令完全只读：不会卸载程序、修改 PATH、改写配置或替用户选择版本。没有安装冲突时退出码为 `0`；发现至少一项冲突时退出码为 `1`，方便脚本和 CI 判断。某个可选内置工具尚未安装或不支持当前平台会显示为 `not found` / `unsupported`，但不会单独令命令失败。

候选路径查找和版本探测使用 dvup 内部的有界并发执行器，最多同时运行 8 项，并对相同路径与参数的探测去重；报告仍严格保持配置顺序和 PATH 顺序。Windows 原生 `.exe`/`.com` 会直接执行，不会为每个版本查询额外启动 PowerShell；`.ps1`/`.cmd` 仍使用对应 shell 以保持解析语义。该优化不调用或依赖 Everything、`Everything64.exe`、`es.exe`、SDK、DLL 或常驻索引服务。

Doctor 会按安装族归并包管理器自身产生的多个入口：`cargo run` 临时加入 PATH 的当前 Cargo build 与同 profile 的 `deps` 产物视为一套开发构建；`~/.cargo/bin` 中的 rustup 代理与同一用户根下 `~/.rustup/toolchains/.../bin` 的实际工具链视为一套 rustup 安装。这些托管路径不会被误报为应删除的重复安装；不同管理器或普通目录中的同版本副本仍会分别显示，例如 Hermes 与 `~/.local/bin` 中的两份 uv。

## 添加自己的更新命令

不需要编辑 TOML：

```console
dvup add claude claude update
```

之后可以像内置工具一样使用：

```console
dvup update claude
dvup list
```

自定义命令保存在用户级 dvup 数据目录，与内置预置自动合并，在任何工作目录都可用。默认会等待与工具同名的进程退出；因此从 Claude Code 中安排 `claude update` 时，任务会先进入后台，等当前 `claude` 进程退出后再更新它的二进制文件。

`dvup add <name> ...` 会在用户 TOML 中把更新命令保存为 `update = [...]`，并把安装探针明确保存为 `probe = ["<name>", "--version"]`。如果显示名称、实际可执行文件或版本参数不同，可以在 TOML 编辑器中直接修改 `probe` 数组；dvup 不会在加载时猜测或回退到其他探针。

删除自定义命令：

```console
dvup remove claude
```

替换同名命令或覆盖内置预置时需要显式确认：

```console
dvup add --force claude claude update
```

Windows 上所有工具命令统一通过系统 `powershell.exe` 执行，因此 PowerShell 能识别的原生程序、`.ps1`、别名、函数和 cmdlet 都能作为自定义命令。执行策略只在该 PowerShell 子进程内设置为 `Bypass`，避免 npm、Scoop 等合法脚本被本机默认策略误拦截。

Linux/macOS 的自定义命令直接执行程序并逐项传递参数，不经过 `/bin/sh`，因此包名、路径和 `--cask` 等选项不会被再次解释。程序会从当前 `PATH` 查找，也支持 `/opt/homebrew/bin/brew`、`/home/linuxbrew/.linuxbrew/bin/brew` 这类绝对路径。内置的 Bun 和 uv 官方安装器会按各自官方方式使用受信任的 shell 安装脚本。

## 使用 npm、pnpm 或 Bun 更新包

`npm` 和 `pnpm` 是执行更新的包管理器，不是 dvup 默认更新的目标。dvup 不会自动执行 `npm install --global npm@latest` 或 `pnpm self-update`。

要更新一个 npm 全局包，添加这个具体包即可：

```console
dvup add codegraph npm install --global @colbymchenry/codegraph@latest
dvup update codegraph
```

使用 pnpm 管理的全局包也是相同方式：

```console
dvup add example-package pnpm add --global package-name@latest
dvup update example-package
```

使用 Bun 管理的全局包：

```console
dvup add example-bun-package bun add --global package-name@latest
dvup update example-bun-package
```

通过 npm 或 pnpm 添加的命令会自动使用 `node-global` 资源组；Bun 包命令使用独立的 `bun-global` 资源组。选择多个使用同一安装目录的包时，dvup 会让它们安全地排队。不同包管理器和其他工具仍然可以并行更新。添加操作本身仍然只保存，不会立即执行包更新。

## 使用 Homebrew 更新软件包

内置的 `brew` 工具执行 `brew update`，用于拉取 Homebrew 自身及所有已配置 tap 的最新元数据：

```console
dvup update brew
```

`brew upgrade` 不会无范围执行。更新具体 formula 或 cask 时，为目标软件包保存一条独立命令。

更新一个 formula：

```console
dvup add ripgrep brew upgrade ripgrep
dvup update ripgrep
```

更新一个 cask：

```console
dvup add zed brew upgrade --cask zed
dvup update zed
```

通过 `brew`、`/opt/homebrew/bin/brew` 或 Linuxbrew 路径添加的命令会自动限定为 macOS/Linux，并共享 `homebrew` 资源组。多个 Homebrew 软件包不会同时修改 Homebrew 数据库，但可以和 npm、Bun、rustup 等任务并行。TUI 中的添加流程同样只保存命令，不会立即运行。

## 内置工具

| 名称 | 执行命令 | 平台 |
| --- | --- | --- |
| `dvup` | `cargo install dvup --locked`，始终在后台更新自身 | Windows、macOS、Linux |
| `bun` | Windows 使用官方 `install.ps1`；macOS/Linux 使用官方 Bash 安装器 | Windows、macOS、Linux |
| `brew` | `brew update`，拉取 Homebrew 自身及软件源 | macOS、Linux |
| `deno` | `deno upgrade` | Windows、macOS、Linux |
| `mise` | `mise self-update` | Windows、macOS、Linux |
| `pixi` | `pixi self-update` | Windows、macOS、Linux |
| `rustup` | `rustup update` | Windows、macOS、Linux |
| `scoop` | `scoop update` | Windows |
| `uv` | Windows 使用 Astral 官方 `install.ps1`；macOS/Linux 使用官方 `install.sh` | Windows、macOS、Linux |

每个内置项都显式声明目标探针和更新执行器。例如 uv 使用 `uv --version` 判断目标是否安装，再调用对应平台的官方安装器更新；只有更新器存在而目标不存在时不会误报为已安装。目标不存在、更新执行器不存在和平台不支持是三种独立状态。`dvup update` 会跳过不能更新的工具，单项失败不会阻止后续工具，最后统一汇总结果。`codex`、`claude`、`opencode` 和 `hermes` 不属于内置工具，只在用户模板中提供注释示例。

## 并行更新与最终报告

独立工具会并行更新，不需要等待前一个工具完成。可能修改同一安装目录的工具会自动串行：

- 启用用户模板中的 `codex` 后，它和其他通过 npm/pnpm 添加的包更新共享 `node-global` 资源组；
- Bun 自身和通过 Bun 添加的全局包共享 `bun-global` 资源组；
- 内置 `brew update` 和所有 Homebrew/Linuxbrew 包共享 `homebrew` 资源组；
- Scoop 应用共享 `scoop` 资源组；`rustup` 使用独立资源组；
- 其他自定义工具默认以工具名作为资源组；
- 高级配置可以用 `resource_group = "name"` 声明两个工具不能同时运行。

并行执行期间不会把多个命令的日志交错输出。所有任务结束后统一显示结果：

```text
RESULTS
STATUS     TOOL                   TIME  DETAIL
UPDATED    bun                    1.2s  Bun official installer
QUEUED     codex                  0.1s  job ...: waiting on process policy
SKIPPED    scoop                  0.0s  not supported on linux
FAILED     codegraph              0.8s  update command returned a non-zero exit status

FAILURE: codegraph
  command:  npm install --global @colbymchenry/codegraph@latest
  resource: node-global
  reason:   update command returned a non-zero exit status
  exit:     1
  stderr:
    ...package-manager error...

SUMMARY: 1 updated, 1 queued, 1 skipped, 1 failed in 1.3s
```

失败详情会明确指出：

- 哪个工具失败；
- 实际执行了什么命令；
- 哪个资源组参与互斥；
- 失败原因和退出码；
- stdout/stderr 的末尾内容；
- 哪些后台任务已经排队，应该使用什么命令继续查看。

显式更新单项：

```console
dvup update bun
dvup update brew
dvup update rustup
dvup update scoop
dvup update uv
```

## 给工具传参数

工具名后面的参数会追加到预置命令：

```console
dvup update scoop zedg
```

等价于：

```console
scoop update zedg
```

多个应用：

```console
dvup update scoop zedg git 7zip
```

参数以 `-` 开头时，可以用 `--` 明确分隔：

```console
dvup update some-tool -- --special-option
```

## 运行中的工具

dvup 会自动处理占用中的工具：

- 等待不能安全终止的进程退出；
- 只终止命令行能确认属于目标工具的 Node 进程；
- 在后台 worker 中处理终止和重试，避免先杀掉调用者；
- 只有共享资源组的更新会串行，其他工具继续并行；
- 遇到 `EBUSY`、sharing violation、文件被占用或 npm 的锁相关 `EPERM` 时自动重试。

Windows、macOS 和 Linux 都会记录 PID、进程名与启动时间，避免 PID 被系统复用后误伤新进程。终止策略在 Unix 上先发送 `SIGTERM`，等待配置的宽限时间后才对仍存活的同一进程实例强制终止。

不会按名称直接终止机器上的所有 `node.exe`。

TUI 标题会显示当前进程策略：

- `WAIT`（默认）：等待配置为 wait 的匹配进程退出；
- `TERMINATE`：把安全的 wait 规则转换为 terminate，由 detached worker 先停止精确匹配的进程，再执行更新。

按 `Shift+Tab` 切换策略。普通 `Tab` 不切换视图，只在添加命令弹窗中切换 Name/Command 输入字段；视图只能使用 `←` 和 `→` 切换。切到 `TERMINATE` 时，策略也会应用到当前状态目录中处于 Pending/Waiting 的任务：TUI 按 PID、进程名和启动时间直接停止精确匹配的进程，原 worker 随后继续原任务，因此旧版本创建的排队任务也不需要再启动第二个更新。切回 `WAIT` 只影响之后创建的新任务，不会撤销已经下达给后台任务的终止指令；已经处于 Running 的任务不会被策略切换打断。

无过滤条件的全局 Node 终止始终被拒绝；只有带 `command_contains` 的 Node 规则才能转换为 terminate。终止模式可能关闭正在使用的 CLI/Agent，确认弹窗会用红色显示这一策略。

启用用户模板中的 `codex` 示例后，CLI 中可以显式使用同一策略：

```console
dvup update codex --terminate-locking-processes
```

## 后台任务

默认状态目录只包含一层应用名称：Windows 使用 `%LOCALAPPDATA%\dvup\data`，Linux 使用 `$XDG_DATA_HOME/dvup`（未设置时为 `~/.local/share/dvup`），macOS 使用 `~/Library/Application Support/dev.dvup`。不会生成 `dvup/dvup` 或 `dvup\dvup`。TUI 设置保存在该目录的 `settings.toml`，可以通过 `DVUP_STATE_DIR` 或全局 `--state-dir` 覆盖并隔离整个状态目录。

默认策略是 `auto`：需要等待、终止或重试时自动转入后台。

```console
# 总是后台执行
dvup update --background always

# 只允许前台执行
dvup update rustup --background never

# 查看任务
dvup jobs
dvup jobs <job-id> --log
```

PowerShell 自动化中可以直接捕获输出：

```powershell
$result = & dvup update
```

Windows worker 不继承调用方的终端或管道句柄，因此后台任务不会阻止输出捕获结束。TUI 子进程、PowerShell 和工具命令统一使用 `CREATE_NO_WINDOW` 在后台无窗口运行，不会弹出新的终端窗口；stdout/stderr 仍会被捕获并显示在 Activity。不要用 `Start-Process -Wait` 等待可能排队的更新；PowerShell 的 `-Wait` 会等待整个后代进程树，而后台 worker 按设计可能继续运行。

Linux/macOS worker 会关闭标准输入、输出和错误句柄，并通过新的 Unix session 脱离调用终端。前台 TUI 子进程的输出仍通过管道返回 Activity，排队任务则写入持久化 Job 日志。

`dvup update --all` 仍然可用，与无参数的 `dvup update` 等价。

## 可选高级配置

普通用户不需要 TOML；`dvup add` 会直接维护全局用户配置。需要手工编辑完整用户清单时可以运行：

```console
dvup init
```

该命令在用户数据目录创建一份干净的全局 `dvup_custom.toml`，其中不会复制任何内置工具。用户模板位于 [configs/dvup.user.example.toml](configs/dvup.user.example.toml)；[configs/dvup.example.toml](configs/dvup.example.toml) 是编译进二进制的完整内置清单，只供 dvup 自身维护。

配置按以下顺序合并，同名工具由后者替换：

```text
二进制内置工具 → 用户数据目录 dvup_custom.toml
```

显式传入 `--config` 时，以二进制内置工具为基础，再应用指定的用户清单；此时不会同时加载全局 `dvup_custom.toml`。dvup 不会自动扫描当前目录或父目录中的 TOML 文件，也不会把运行时合并结果写回磁盘。

用户清单采用独立的简洁模型。`update` 和 `probe` 都是必填命令数组，第一个元素是程序，其余元素是参数：

```toml
version = 1

[tools.example]
update = ["package-manager", "update", "example"]
probe = ["example", "--version"]
```

`background` 默认为 `auto`，标准锁等待、超时和重试值也不会写入文件。只有改变标准行为时才需要高级字段。用户清单与二进制内置清单使用各自独立且严格的结构，未知字段会直接报错；dvup 不包含格式迁移或兼容解析。

Codex、Claude Code、OpenCode 和 Hermes 都是用户自定义示例，不会随 dvup 内置启用。可以从用户模板复制或取消注释所需项：

```toml
[tools.codex]
update = ["npm", "install", "--global", "@openai/codex@latest"]
probe = ["codex", "--version"]
resource_group = "node-global"

[[tools.codex.processes]]
name = "node"
command_contains = "@openai/codex"
action = "terminate"

[tools.claude]
update = ["claude", "update"]
probe = ["claude", "--version"]

[tools.opencode]
update = ["opencode", "upgrade"]
probe = ["opencode", "--version"]

[tools.hermes]
update = ["hermes", "update"]
probe = ["hermes", "--version"]
```

持久加入 `scoop update zedg`：

```toml
[tools.scoop-zedg]
update = ["scoop", "update", "zedg"]
probe = ["zedg", "--version"]
platforms = ["windows"]
resource_group = "scoop"
```

使用 npm、pnpm 或 Bun 更新具体的全局包：

```toml
[tools.codegraph]
update = ["npm", "install", "--global", "@colbymchenry/codegraph@latest"]
probe = ["codegraph", "--version"]
resource_group = "node-global"

[tools.example-pnpm-package]
update = ["pnpm", "add", "--global", "package-name@latest"]
probe = ["package-name", "--version"]
resource_group = "node-global"

[tools.example-bun-package]
update = ["bun", "add", "--global", "package-name@latest"]
probe = ["package-name", "--version"]
resource_group = "bun-global"
```

使用 Homebrew 更新 formula 或 cask：

```toml
[tools.ripgrep]
update = ["brew", "upgrade", "ripgrep"]
probe = ["ripgrep", "--version"]
platforms = ["macos", "linux"]
resource_group = "homebrew"

[tools.zed]
update = ["brew", "upgrade", "--cask", "zed"]
probe = ["zed", "--version"]
platforms = ["macos", "linux"]
resource_group = "homebrew"
```

默认会等待与工具名称相同的进程；使用 `wait_for` 可以明确替换这一列表，空数组表示不添加默认等待规则：

```toml
[tools.rust-nightly]
update = ["rustup", "update", "nightly"]
probe = ["rustc", "+nightly", "--version"]
wait_for = ["cargo", "rustc"]
```

如果两个自定义工具会修改同一个目录，让它们共享资源组：

```toml
[tools.first]
update = ["manager-a", "update"]
probe = ["first", "--version"]
resource_group = "shared-sdk"

[tools.second]
update = ["manager-b", "update"]
probe = ["second", "--version"]
resource_group = "shared-sdk"
```

需要临时使用另一份全局工具清单时，可以显式指定文件：

```console
dvup update --config path/to/dvup_custom.toml
```

## 构建与验证

```console
cargo build --release
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

仓库 CI 会在 `ubuntu-latest`、`macos-latest` 和 `windows-latest` 上分别执行格式检查、Clippy、全部测试和 release 构建，确保三个平台的条件编译路径持续可用。

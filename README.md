# dvup

一个零配置、跨平台、能处理运行中进程、文件占用和包管理器资源锁的工具链更新器，支持 Windows、macOS 和 Linux。

## 开始使用

直接运行即可进入交互界面，不需要准备配置文件：

```console
dvup
```

界面会列出所有内置和自定义工具、安装状态、已安装版本、最新版本、更新命令（或简短友好名称）与最近一次结果。已安装版本通过本地只读探针获取，最新版本只从工具显式配置的 npm、PyPI、crates.io 或 GitHub 来源查询，两者都在后台异步回填，不会阻塞 TUI；没有配置来源或无法识别时显示 `—`。用方向键选择工具，按 `Enter` 确认后执行常规更新；已安装版本与最新版本完全相同的工具会直接标记为 `up to date / 已是最新`，不会再次启动更新器。也可以用 `Space` 选择多项后一起并行更新。

版本探测只会为当前平台受支持且目标已经安装的工具启动；`missing` 和 `unsupported` 不执行本地版本命令，也不会发起最新版本网络请求。目标已经安装但仅缺少更新器的工具仍会查询版本，以便准确展示当前状态。

最新版本请求不会再把所有失败都显示成 `—`：GitHub 或其他版本源返回 403/429 时，工具行显示 `rate limited / 已限流`；Token 失效、仓库或 Release 不存在、网络/代理失败分别显示 `auth failed / 认证失败`、`not found / 未找到`、`fetch failed / 获取失败`。底部状态栏同时给出检查 Token、等待配额重置、检查仓库配置或按 `r` 重试等对应提示，错误文本不会包含 Token 或认证头。

| 按键 | 操作 |
| --- | --- |
| `↑` / `↓`、`j` / `k` | 移动选择或滚动日志 |
| `Space` | 在 Tools 页选择或取消命令工具/GitHub 仓库；在 Settings 页切换选项 |
| `a` | 选择或取消全部已安装工具 |
| `Enter` | 更新尚未达到最新版本的命令工具；在 GitHub 仓库视图确认后安装所选 Release；在 Jobs 页展开或收起任务结果；在 Doctor 页首次启动扫描或展开结果；在 Settings 页切换选项 |
| `Tab` | 在 Tools 页的“命令工具”和“GitHub 仓库”视图之间切换 |
| `v` | 在 Tools 页为当前工具输入一个精确目标版本；仅对配置了 `update_version` 的工具可用 |
| 鼠标左键 | 点击顶部标签页或 Tools 内部视图切换；在 Tools 页勾选命令工具/GitHub 仓库；在 Activity 页展开执行输出；在 Jobs 页展开任务结果；在 Doctor 页展开诊断详情；在 Settings 页切换选项 |
| 鼠标移动 | Tools/Jobs/Doctor/Settings 行焦点跟随鼠标；标签页和 Activity 执行标题显示悬停高亮，不自动触发操作 |
| 鼠标滚轮 | 每格滚动一行；Tools/Jobs/Doctor 列表滚动后焦点保持在鼠标所在屏幕行；详情区、Activity 和 TOML 编辑器逐行滚动内容 |
| `PgUp` / `PgDn` | 在 Jobs 页滚动已展开的任务结果 |
| `t` | 在 Tools 页打开当前生效配置的 TOML 编辑视图 |
| `o` | 在 Tools 页使用系统文本编辑器直接打开同一个 TOML 配置文件 |
| `c` | 添加自定义更新命令 |
| `e` | 编辑或重命名选中的自定义命令 |
| `d` | 删除选中的自定义命令（内置工具不能删除） |
| `→` | 切换到下一个页面 |
| `←` | 切换到上一个页面 |
| `Shift+Tab` | 在 `WAIT` 与 `TERMINATE` 进程策略间切换 |
| `L` | 在中文与英文界面间切换 |
| `r` | 重新读取并应用当前 TOML 配置，再刷新当前视图；在 Doctor 页随后启动或重新执行诊断扫描 |
| `Ctrl+C` | 唯一的退出快捷键：第一次显示提醒，连续第二次退出；TOML 编辑视图中改为复制选区，不触发退出 |

已安装版本与最新版本相同时，最新版本显示为绿色；不同时显示为黄色，但 dvup 不用字符串大小关系猜测哪个版本更新。更新完成后，Tools 页会重新探测两种版本，并显示每项的 `updated`、`queued` 或 `failed` 状态和耗时；后台 Job 成功完成时也会重新探测。Bun、uv 等内置工具在日常界面中使用简短的友好名称，不展示冗长的底层安装命令。TUI 顶部状态栏显示当前本地日期时间。Activity 页保留退出码、stdout/stderr 和整批汇总，每条记录都显示产生时的本地日期时间；每次执行默认折叠，点击带 `▸` 的执行标题可原地展开输出，并使用青色表示启动、绿色表示成功、黄色表示排队或人工处理提示、红色表示失败、蓝色表示任务元数据。Jobs 页显示任务更新时间，点击对应任务（或按 `Enter`）会在当前页面展开结果，再次点击即可收起，不会跳转到 Activity。新写入的持久化 Job 日志会为每行 worker 状态和命令输出添加日期时间；`dvup jobs` 列表和详情也会显示任务时间。外部命令的 ANSI 控制序列、回车覆盖和纯进度条会在渲染前清理，避免动态终端输出破坏 TUI 布局；普通日志和失败正文仍会完整保留。失败诊断和 Jobs 任务详情仍保留实际命令，方便排错。也可以显式使用 `dvup tui` 启动界面。

Doctor 页提供与 `dvup doctor` 相同的安装冲突诊断。默认情况下，进入 TUI 或切换到该页都不会自动执行诊断；从未扫描时会明确显示提示，按 `Enter` 开始首次扫描，之后按 `r`/`R` 主动重新扫描。扫描在后台运行，不会阻塞界面，扫描期间再次按 `Enter` 或 `R` 不会创建重复任务。表格显示每个工具的状态、当前生效路径、版本、安装数量和更新器。已有结果时，点击工具行或按 `Enter` 会在当前页面展开 active/shadowed 完整路径、安装来源、版本差异和处理建议，再次操作即可收起；鼠标滚轮或 `PgUp`/`PgDn` 可以滚动长详情。标题会显示最近检查的本地日期时间；工具更新、配置增删改或后台 Job 完成不会隐式覆盖结果。

Settings（设置）页提供启动诊断、工具过滤和应用级网络策略。使用上下方向键或鼠标移动焦点，再用鼠标点击、`Space` 或 `Enter` 操作。修改会立即保存；自动诊断选项不会在当前会话中突然启动扫描，而是从下次进入 TUI 起在后台自动运行一次只读诊断。这个选项只影响 TUI 启动，不会让“切换到 Doctor 页”重新成为扫描触发条件；Doctor 页的 `Enter` 和 `R` 手动扫描始终可用。工具过滤选项只在内存中根据已有状态重建 Tools 和 Doctor 的可见行，不会重新扫描 PATH、启动 PowerShell 或重复执行版本探测，因此会立即响应；它只隐藏 `unsupported` 和 `missing`，目标已安装但仅缺少更新器的工具仍会保留，关闭后按原顺序恢复完整列表。按 `L` 选择的界面语言也会立即写入同一设置文件，并在下次启动时恢复。设置保存在状态目录的 `settings.toml` 中，使用 `--state-dir` 时会随指定状态目录隔离。

网络策略有三个严格且互斥的模式：

- `environment`：继承启动 dvup 的进程环境中的 `ALL_PROXY`、`HTTPS_PROXY`、`HTTP_PROXY` 与 `NO_PROXY`（同时识别小写形式）。适合终端、systemd 或容器统一注入代理。
- `explicit`：只使用 Settings 中填写的一个 HTTP/HTTPS CONNECT 代理地址，并把同一地址和逗号分隔的 `no_proxy` 同时用于 dvup 的版本查询和它启动的更新命令。此模式不会再读取环境代理。
- `direct`：dvup 的 HTTP 请求显式关闭代理，并从子命令环境中移除全部大小写代理变量。

三种模式都不会在代理连接失败后改为直连，也不会退回另一个模式。首个版本只接受 `http://` 和 `https://`，明确拒绝 SOCKS、带用户名或密码的代理 URL 以及无效的绕过规则。Settings 中的“测试仓库连接”会分别请求 npm、PyPI、crates.io 和 GitHub，并显示每个端点的耗时或具体错误，不会用一个笼统的“网络正常”隐藏部分失败。已经创建的前台或后台任务保存自己的网络设置快照；之后修改 Settings 只影响新任务和新版本查询。

在 Settings 聚焦“代理模式”并按 `Enter` 或 `Space` 会打开统一的网络代理窗口。使用 `←`/`→` 或 `Space` 在 `environment`、`explicit`、`direct` 之间切换；`environment` 和 `direct` 可直接按 `Enter` 保存，`explicit` 按 `Enter` 后依次编辑代理地址和绕过规则。任意栏均可按 `Ctrl+S` 保存，按 `Esc` 取消；鼠标点击模式栏也可以循环切换。窗口操作提示分两行显示，在较窄的中英文终端中不会截断保存或取消快捷键。

`settings.toml` 使用严格 schema，未知字段和缺失的必填字段都会直接报错。默认配置保持干净，只记录当前设置和环境代理模式：

```toml
language = "chinese"
auto_diagnose_on_startup = false
hide_unsupported_and_missing_tools = false

[network]
proxy_mode = "environment"

[github]
poll_interval_secs = 1800
```

显式代理配置如下；`proxy_url` 和 `no_proxy` 在 `environment` 或 `direct` 模式下出现会直接报错：

```toml
[network]
proxy_mode = "explicit"
proxy_url = "http://127.0.0.1:7890"
no_proxy = ["localhost", "127.0.0.1", ".internal.example"]
```

Settings 的 `GitHub API Key` 行使用遮罩输入。保存时只把 `encrypted_api_key` 密文写入 `settings.toml`，应用重启后自动解密，不需要重新输入；明文不会写入配置、任务日志、Activity、子进程环境或命令行。Windows 使用绑定当前 Windows 用户的原生 DPAPI 加密，因此复制密文到其他账号无法解密；macOS/Linux 使用 AES-256-GCM，随机加密密钥分别由 Keychain/Secret Service 保存。这里没有旧凭据迁移或明文兼容路径，格式错误、密文损坏或不属于当前用户都会直接报错。Windows 上如果编辑器、安全软件或其他进程短暂占用 `settings.toml`，原子替换会先进行有限重试；最终仍无法写入时，TUI 会显示被占用的精确配置路径并保留遮罩输入，释放占用后可直接按 `Enter` 重试，不会误报为凭据或加密配置错误。保存后 GitHub Release/Tag 最新版本查询会立即重新执行，以避免匿名 API 每小时 60 次的限额。TUI 顶部会通过一次后台 `/user` 请求同时显示 `@Token主人`、已用和剩余 API 配额，并用进度条按剩余比例切换绿/黄/红警示；状态每 5 分钟低频刷新，Token 不会进入显示文本或错误信息。

GitHub Release 监控使用严格配置。在 Tools 页按 `Tab` 切到“GitHub 仓库”，表格会直接显示仓库、真实已安装版本、最新 tag、更新状态、更新策略和目标目录；`A` 新增、`E` 编辑、`D` 删除、`Space` 选择、`Enter` 确认并安装所选 Release，`R`/`C` 刷新状态。DMG 监控从目标 `.app/Contents/Info.plist` 读取 `CFBundleShortVersionString`，缺失时回退到 `CFBundleVersion`；应用不存在时显示未安装，且比较时忽略 GitHub tag 常见的前导 `v`。其他资产格式仍使用 dvup 上次成功安装的 tag。新增和编辑表单会与自定义命令工具一起原子写入 `dvup_custom.toml`，校验失败会保留全部输入供直接修正；`settings.toml` 严格拒绝 `github.monitors`，Settings 只保留 GitHub API Key、轮询间隔和网络策略等全局设置。每项监控可选择 `update_policy = "manual"`（默认，需要逐次确认）或 `update_policy = "automatic"`（后台探测到新 tag 后立即安装）；停用项、探测失败项和已是最新版的项不会进入自动安装队列。`asset_regex` 使用 Rust `regex` 语法，推荐用 `^` 和 `$` 限定完整文件名；正则必须恰好匹配一个 Release asset。`target_directory` 必须是绝对路径。以下配置写入 `dvup_custom.toml`：

```toml
[[github.monitors]]
name = "reqable-macos-arm64"
repository = "reqable/reqable-app"
asset_regex = '^reqable-app-macos-arm64\.dmg$'
target_directory = "/Applications/Reqable.app"
format = "dmg"
update_policy = "automatic"
cleanup_installer = true
max_download_bytes = 104857600
max_extracted_bytes = 536870912
max_extracted_files = 20000
strip_components = 0
enabled = true

[[github.monitors]]
name = "reqable-windows-x64"
repository = "reqable/reqable-app"
asset_regex = '^reqable-app-windows-x86_64\.zip$'
target_directory = 'C:\Tools\Reqable'
format = "zip"
max_download_bytes = 104857600
max_extracted_bytes = 314572800
max_extracted_files = 1000
strip_components = 0
enabled = true

[[github.monitors]]
name = "wsl-dashboard-windows-x64"
repository = "owu/wsl-dashboard"
asset_regex = '^WSLDashboard\.[0-9]+\.[0-9]+\.[0-9]+\.Setup\.x64\.zip$'
target_directory = 'C:\Tools\WSLDashboard'
format = "zip"
max_download_bytes = 104857600
max_extracted_bytes = 314572800
max_extracted_files = 1000
strip_components = 0
enabled = true
```

`cleanup_installer` 默认为 `true`：安装前清理该监控以前保留的安装包，本次临时下载也会在安装结束后自动删除。设为 `false` 时，原始 Release asset 会保留在状态目录的 `github-installers/<监控名>/<资产名>`，可用于离线重装。`format` 只能是 `file`、`zip`、`tar_gz` 或 macOS 专用的 `dmg`，不会在格式错误时猜测或尝试另一种处理方式。每个监控项必须明确限制下载字节数；ZIP、TAR.GZ 和 DMG 还必须限制展开总字节数和文件数。普通 `file` 的两个展开上限必须为 `0`。ZIP/TAR.GZ 会先下载和解压到目标目录同一文件系统中的临时目录，校验路径不能越界，拒绝 TAR 符号链接等非普通条目，再替换完整目标目录；失败时保留原目录。DMG 目标必须是绝对 `.app` 路径：dvup 使用 `hdiutil` 只读挂载镜像，只接受唯一的顶层 `.app`，检查展开上限，使用 `ditto` 保留应用包元数据，通过 `codesign --verify --deep --strict` 后卸载镜像，最后在同一文件系统中原子替换旧应用；任何前置步骤失败都不会修改现有应用。普通 `file` 会保存为 Release asset 的原始文件名。成功安装的 tag 单独记录在状态目录的 `github-releases.json`，其中不包含 API Key；对 DMG 而言它只作为安装历史，真实版本始终以目标应用的 `Info.plist` 为准。

服务器最简单的配置方式是保留 `environment` 模式并在服务环境中注入标准变量，例如：

```console
HTTP_PROXY=http://proxy.internal:3128
HTTPS_PROXY=http://proxy.internal:3128
NO_PROXY=localhost,127.0.0.1,.internal.example
```

systemd 可以把这些值写入 unit 的 `Environment=` 或 `EnvironmentFile=`，Docker 可以使用 `-e HTTP_PROXY=... -e HTTPS_PROXY=... -e NO_PROXY=...`。外部包管理器是否读取这些标准变量仍由该工具自身决定；dvup 会准确注入所选策略，但不会伪装或重试不支持代理的第三方命令。

TUI 使用统一的暗色语义配色、圆角弱边框、选中行底色和滚动条。确认更新、添加命令、确认保存与删除窗口会显示为居中的独立面板，并压暗底层视图；子窗口存在期间，键盘和鼠标输入只交给当前子窗口，不能切换标签页、勾选工具、展开 Activity/Jobs、滚动底层内容、切换进程策略或退出程序。普通确认框和单行表单可用 `Esc` 或 `Ctrl+C` 关闭/返回，确认操作仍使用 `Enter`/`y`；TOML 编辑视图中的 `Ctrl+C` 专用于复制，不会关闭窗口或退出 dvup。

在 Tools 页按 `t` 会打开全屏 TOML 编辑视图。使用 `--config` 时编辑该显式文件；否则始终编辑用户数据目录中的全局 `dvup_custom.toml`，不会扫描或创建当前项目配置。目标文件尚不存在时，编辑器会用当前聚焦工具生成一份可通过校验的初始配置，直到按 `Ctrl+S` 才真正创建文件。编辑器使用 Taplo 语法树高亮键名、字符串、数字、布尔值、日期时间、注释和错误标记；即使文件尚未写完或语法无效，也会保留并高亮全部原始内容。编辑器支持方向键、`Home`/`End`、`PgUp`/`PgDn` 移动光标，按住 `Shift` 扩展选区，鼠标点击定位、拖拽选择、滚轮滚动；`Ctrl+A` 全选，`Ctrl+C` 复制原始 TOML 选区，`Ctrl+V` 或终端原生多行粘贴会替换选区。`Ctrl+/` 会对当前行或选区覆盖的所有完整行统一增加或移除 `# `，并保留整行选区；`Ctrl+Z` 撤销，`Ctrl+Y` 或 `Ctrl+Shift+Z` 重做。输入批次、多行粘贴和整块注释各自只占一个撤销步骤，历史最多保留 100 步且总快照约束为 8 MiB。终端把粘贴拆成连续字符事件时，dvup 会合并为批量文本插入，避免逐字符解析和重绘。`Ctrl+S` 会先执行完整 TOML 与 dvup 配置校验，再原样保存注释和排版并刷新工具列表；无效内容不会覆盖磁盘文件。按 `Esc` 关闭编辑视图。

内置 TOML 编辑器按 `F2` 可在标准模式与 Vim 模式之间切换，标题会明确显示 `STANDARD`、`VIM NORMAL`、`VIM INSERT` 或 `VIM VISUAL`。Vim 模式支持 `h/j/k/l`、`w/b`、`0/$`、`gg/G` 导航，`i/a/I/A` 进入插入，`v/V` 选择，`x`、`dd`、`yy`、`p` 编辑，以及 `u`、`Ctrl+R` 撤销和重做；`Esc` 从 INSERT/VISUAL 返回 NORMAL，`Ctrl+S` 保存，`Ctrl+Q` 关闭。因此在没有鼠标的 SSH/服务器终端中也能完成编辑。Tools 页按 `o` 会直接打开同一个统一配置路径：Windows 使用系统记事本，macOS 使用系统文本编辑器，Linux 使用默认文件关联程序；文件不存在时会先写入当前聚焦工具生成的有效初始配置，不会创建项目目录下的另一份配置。

也可以把一个已经存在的 `.toml` 文件直接传给 dvup，跳过 Tools 页面并立即进入编辑器；Windows 文件关联或把 TOML 文件拖到 `dvup.exe` 上时使用的是同一入口：

```console
dvup path/to/dvup_custom.toml
```

直达模式只在启动时检查文件存在且扩展名为 `.toml`，不会要求内容已经是有效配置，因此可以直接打开语法损坏或缺少字段的文件进行修复。只有按 `Ctrl+S` 时才执行完整 dvup 配置校验；校验失败仍不会覆盖原文件。

TUI 默认使用英文，随时按 `L` 可切换为中文，再按一次切回英文。标签页、表头、工具与任务状态、帮助栏、表单、确认框、进程策略以及 dvup 在界面内生成的运行摘要都会即时使用当前语言；已经写入 Activity 的历史记录保留产生时的语言，外部工具 stdout/stderr 和持久化 Job 日志始终保持原文，确保诊断内容不被翻译改写。在添加命令、目标版本文本输入框和 TOML 编辑视图中，`l`/`L` 仍作为普通字符输入；退出输入界面后即可继续用 `L` 切换语言。

在 TUI 中按 `c`，只需填写名称和命令。例如名称填写 `claude`，命令填写 `claude update`；也可以填写 `npm install -g package@latest`、`pnpm add -g package@latest` 或 `brew upgrade ripgrep`。填写后会先出现预览确认框，并明确提示“只保存，不执行”。保存完成后界面返回 Tools 页面并聚焦新工具；只有之后再次按 `Enter` 并确认更新，命令才会真正执行。

添加/编辑表单也支持通过 OpenAI 兼容接口辅助生成配置，不要求本机安装 Codex、Claude Code 或其他 Agent CLI。先在 Settings 页打开合并后的 `AI 生成` 设置，按“启用、Base URL、API Key、模型”的顺序配置：填写 Base URL 和可选 API Key 后按 `Ctrl+R` 获取 `/models` 列表，模型字段会自动带入首个结果，也可以聚焦模型字段后用左右键选择其他结果或直接输入模型名；按 `Ctrl+T` 可以在保存前用当前输入测试接口、鉴权和模型。AI 开关只控制是否允许生成，关闭后仍保留连接信息和已选模型。Base URL 可以填写到 `/v1`，dvup 会自动追加 `/chat/completions`；如果服务地址已经包含 `/chat/completions`，则不会重复追加。远程服务必须使用 HTTPS，HTTP 仅允许 `localhost`、回环 IP 或 `.localhost` 域名，避免明文传输凭据。API Key 使用与 GitHub 凭据相同的操作系统安全存储机制加密，明文不会写入 `settings.toml`；不需要鉴权的本地兼容服务可以留空。留空 API Key 会保留原值，聚焦该字段时按 `Ctrl+D` 可标记删除。

在添加或编辑更新命令时，先填写工具名称或已有命令作为提示，然后按 `Ctrl+G`。dvup 会把当前操作系统、CPU 架构和 PATH 中可用的常见包管理器告诉模型，要求它只返回一条直接执行的更新命令；生成结果会回填表单，仍需用户预览并确认保存，绝不会立即执行。在添加或编辑 GitHub 仓库监控时，先填写 `owner/repo` 或完整 GitHub URL，再按 `Ctrl+G`。dvup 会先从 GitHub 获取该仓库最新 Release 的真实资产名称，再让模型按当前系统和架构选择资产、生成 Rust 正则、格式、用户级目标目录和安全解压上限；只有正则在最新 Release 中恰好匹配一个资产且全部严格配置校验通过时才会回填。用户仍可修改所有字段并决定是否保存。

AI 请求沿用 Settings 中选择的网络/代理策略。发送给模型的环境信息仅限操作系统、架构、可用包管理器、dvup 的用户级安装目录、当前表单提示，以及生成 GitHub 监控时的 Release 资产名称；不会发送现有配置文件、Activity 日志、任务日志或其他环境变量。模型输出始终被视为不可信候选值，必须经过 dvup 本地解析和校验。

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

# 将支持指定版本的工具更新或降级到精确版本
dvup update codex --to 0.143.0

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

候选路径查找和版本探测使用 dvup 内部的有界并发执行器，最多同时运行 8 项，并对相同路径与参数的探测去重；报告仍严格保持配置顺序和 PATH 顺序。Windows 原生 `.exe`/`.com` 会直接执行，`.cmd`/`.bat` 统一交给系统 `cmd.exe`；`.ps1`、PowerShell 别名、函数和 cmdlet 不属于可用命令。该优化不调用 PowerShell，也不依赖 Everything、`Everything64.exe`、`es.exe`、SDK、DLL 或常驻索引服务。

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

Windows 上只解析当前工作目录和 `PATH`/`PATHEXT` 中的原生命令：`.exe`/`.com` 直接启动，`.cmd`/`.bat` 通过系统 `cmd.exe` 启动。dvup 不调用 PowerShell，也不识别 `.ps1`、PowerShell 别名、函数或 cmdlet。内置 Bun 与 uv 使用各自原生的 `bun upgrade` 和 `uv self update`。

Linux/macOS 的自定义命令直接执行程序并逐项传递参数，不经过 `/bin/sh`，因此包名、路径和 `--cask` 等选项不会被再次解释。程序会从当前 `PATH` 查找，也支持 `/opt/homebrew/bin/brew`、`/home/linuxbrew/.linuxbrew/bin/brew` 这类绝对路径。内置的 Bun 和 uv 安装更新仍按平台预置执行。

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

Windows worker 不继承调用方的终端或管道句柄，因此后台任务不会阻止输出捕获结束。TUI 子进程和工具命令统一使用 `CREATE_NO_WINDOW` 在后台无窗口运行，不会弹出新的终端窗口；stdout/stderr 仍会被捕获并显示在 Activity。不要用 `Start-Process -Wait` 等待可能排队的更新；调用方 shell 的进程树等待语义可能把后台 worker 一并算入，而后台 worker 按设计可能继续运行。

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
[tools.example]
update = ["package-manager", "update", "example"]
probe = ["example", "--version"]
```

要显示最新版本，给工具声明一个权威来源。支持的五种严格格式如下；字段名或 provider 写错会直接报错，不会尝试其他服务：

```toml
latest = { provider = "npm", package = "@openai/codex" }
latest = { provider = "pypi", package = "hermes-agent" }
latest = { provider = "crates_io", package = "dvup" }
latest = { provider = "github_release", repository = "denoland/deno" }
latest = { provider = "github_tag", repository = "rust-lang/rustup" }
```

要允许用户更新或降级到任意精确版本，还必须提供独立的 `update_version` 命令模板，并且整个数组中必须恰好出现一次 `{version}`：

```toml
[tools.codex]
update = ["npm", "install", "--global", "@openai/codex@latest"]
probe = ["codex", "--version"]
latest = { provider = "npm", package = "@openai/codex" }
update_version = ["npm", "install", "--global", "@openai/codex@{version}"]
```

之后可在命令行执行 `dvup update codex --to 0.143.0`，或在 TUI 聚焦该工具后按 `v` 输入版本。`--to` 只允许单个工具，不能与 `--all` 同时使用。没有 `update_version` 的工具仍可执行常规更新，但会明确拒绝指定版本；dvup 不会把版本号猜测性地追加到普通 `update` 命令后面。

`background` 默认为 `auto`，标准锁等待、超时和重试值也不会写入文件。只有改变标准行为时才需要高级字段。用户清单与二进制内置清单使用各自独立且严格的结构，未知字段会直接报错；dvup 不包含格式迁移或兼容解析。

Codex、Claude Code、OpenCode 和 Hermes 都是用户自定义示例，不会随 dvup 内置启用。可以从用户模板复制或取消注释所需项：

```toml
[tools.codex]
update = ["npm", "install", "--global", "@openai/codex@latest"]
probe = ["codex", "--version"]
latest = { provider = "npm", package = "@openai/codex" }
update_version = ["npm", "install", "--global", "@openai/codex@{version}"]
resource_group = "node-global"

[[tools.codex.processes]]
name = "node"
command_contains = "@openai/codex"
action = "terminate"

[tools.claude]
update = ["claude", "update"]
probe = ["claude", "--version"]
latest = { provider = "npm", package = "@anthropic-ai/claude-code" }
update_version = ["claude", "install", "{version}"]

[tools.opencode]
update = ["opencode", "upgrade"]
probe = ["opencode", "--version"]
latest = { provider = "npm", package = "opencode-ai" }
update_version = ["opencode", "upgrade", "{version}"]

[tools.hermes]
update = ["hermes", "update"]
probe = ["hermes", "--version"]
latest = { provider = "pypi", package = "hermes-agent" }
```

Hermes 使用 PyPI 的 `hermes-agent` 包版本，与 `hermes --version` 输出的首个版本号属于同一套版本体系。它当前的更新命令没有精确版本参数，所以没有声明 `update_version`。

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

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
| 鼠标左键 | 点击顶部标签页或 Tools 内部视图切换；在 Tools 页勾选命令工具/GitHub 仓库；在添加向导中选择方式、候选和输入位置；在 Activity/Jobs/Doctor 页展开详情；在 Settings 页切换选项 |
| 鼠标移动 | Tools/Jobs/Doctor/Settings 行焦点跟随鼠标；标签页和 Activity 执行标题显示悬停高亮，不自动触发操作 |
| 鼠标滚轮 | 每格滚动一行；Tools/Jobs/Doctor 列表滚动后焦点保持在鼠标所在屏幕行；详情区、Activity 和 TOML 编辑器逐行滚动内容 |
| `PgUp` / `PgDn` | 在 Jobs 页滚动已展开的任务结果 |
| `t` | 在 Tools 页打开当前生效配置的 TOML 编辑视图 |
| `o` | 在 Tools 页使用系统文本编辑器直接打开同一个 TOML 配置文件 |
| `c` | 在命令视图选择“包管理器 / 自定义命令 / AI”，或在 GitHub 视图选择“手动 Release 监控 / AI” |
| `e` | 按当前视图和声明类型打开对应的简化编辑向导 |
| `d` | 删除当前视图中选中的用户命令或 GitHub 监控（内置命令不能删除） |
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

在 Settings 聚焦“代理模式”并按 `Enter` 或 `Space` 会打开统一的网络设置窗口。使用 `←`/`→` 或 `Space` 在 `environment`、`explicit`、`direct` 之间切换；`explicit` 模式额外开放代理地址和绕过规则。三种模式都可以编辑元数据请求、Release Asset 建立连接/等待响应头、Release Asset 正文下载这三项秒级超时。`Tab`、`↑`、`↓` 或 `Enter` 在可用栏之间移动，在最后一栏按 `Enter` 保存；任意栏均可按 `Ctrl+S` 保存，按 `Esc` 取消。鼠标可以循环切换模式、定位并编辑所有可用输入栏。

`settings.toml` 使用严格 schema，未知字段和缺失的必填字段都会直接报错。默认配置保持干净，只记录当前设置和环境代理模式：

```toml
language = "chinese"
auto_diagnose_on_startup = false
hide_unsupported_and_missing_tools = false

[network]
proxy_mode = "environment"
metadata_timeout_secs = 10
release_asset_setup_timeout_secs = 30
release_asset_body_timeout_secs = 300

[github]
poll_interval_secs = 1800
```

显式代理配置如下；`proxy_url` 和 `no_proxy` 在 `environment` 或 `direct` 模式下出现会直接报错：

```toml
[network]
proxy_mode = "explicit"
proxy_url = "http://127.0.0.1:7890"
no_proxy = ["localhost", "127.0.0.1", ".internal.example"]
metadata_timeout_secs = 10
release_asset_setup_timeout_secs = 30
release_asset_body_timeout_secs = 300
```

三个超时键都是 `[network]` 的必填字段，不使用 Serde 默认值，也不会对已有配置执行迁移、兼容解析或 fallback。仅当整个 `settings.toml` 尚不存在时，新配置才从 `10`、`30`、`300` 秒开始。`metadata_timeout_secs` 和 `release_asset_setup_timeout_secs` 的有效范围都是 `1..=300`，`release_asset_body_timeout_secs` 的有效范围是 `1..=3600`，并且正文下载超时不能小于建立连接/等待响应头的超时。零、越界值、非整数或缺失字段都会直接报出配置错误。

Settings 的 `GitHub API Key` 行使用遮罩输入。保存时只把 `encrypted_api_key` 密文写入 `settings.toml`，应用重启后自动解密，不需要重新输入；明文不会写入配置、任务日志、Activity、子进程环境或命令行。Windows 使用绑定当前 Windows 用户的原生 DPAPI 加密，因此复制密文到其他账号无法解密；macOS/Linux 使用 AES-256-GCM，随机加密密钥分别由 Keychain/Secret Service 保存。这里没有旧凭据迁移或明文兼容路径，格式错误、密文损坏或不属于当前用户都会直接报错。Windows 上如果编辑器、安全软件或其他进程短暂占用 `settings.toml`，原子替换会先进行有限重试；最终仍无法写入时，TUI 会显示被占用的精确配置路径并保留遮罩输入，释放占用后可直接按 `Enter` 重试，不会误报为凭据或加密配置错误。保存后 GitHub Release/Tag 最新版本查询会立即重新执行，以避免匿名 API 每小时 60 次的限额。TUI 顶部会通过一次后台 `/user` 请求同时显示 `@Token主人`、已用和剩余 API 配额，并用进度条按剩余比例切换绿/黄/红警示；状态每 5 分钟低频刷新，Token 不会进入显示文本或错误信息。

GitHub Release 监控与命令工具完全隔离。切到 Tools 页的“GitHub 仓库”视图后按 `c`，可以选择“手动添加 Release 监控”或“使用 AI 分析 GitHub 仓库”；命令视图不会创建 GitHub Monitor，GitHub 视图也不会创建包管理器命令。手动流程不需要启用或配置 AI。

手动添加时输入 `owner/repo` 或 GitHub 仓库 URL。dvup 在后台调用 GitHub 官方 API 验证仓库和最新 Release，过滤源码包、checksum、签名、SBOM 以及与当前 OS/CPU 不兼容的资产，然后显示该 Release 的真实 Asset 列表。用户必须明确选择一个文件；dvup 会生成包含产品、系统、架构、格式和可选变体（例如 `musl`、`gnu`、`portable`）的语义选择器，并立即回验它只命中所选文件。零匹配会报告“没有兼容的发布文件”，多匹配会要求重新选择或填写区分变体；不会取第一个文件、最高分文件，也不会回退到其他系统、架构、格式或仓库。相同的唯一匹配规则会应用于以后每个 Release，包括自动更新策略。

普通文件、ZIP 和 TAR.GZ 安装到 dvup 用户目录下的 `github-tools/<监控名>`。压缩包会先完整解压到临时目录：只有一个公共顶层目录时自动去掉这一层，否则保留原目录结构；下载大小、展开大小和文件数量使用程序内固定安全上限。安装成功或失败后都会清理临时下载与解压目录。DMG 需要再输入一个 `.app` 名称，运行时目标固定编译为 `/Applications/<Application>.app`。保存监控只写配置，不下载、不挂载、不解压、也不安装任何 Asset。

GitHub 元数据与 Release Asset 使用两套独立网络策略：Release、Tag、用户和配额等小型元数据请求使用 `metadata_timeout_secs` 作为端到端超时；Asset 下载不设元数据请求的总时长上限，而是使用 `release_asset_setup_timeout_secs` 分别限制域名解析、连接、请求发送和响应头等待，并使用 `release_asset_body_timeout_secs` 限制响应体传输。调用方只选择“元数据”或“Release Asset”策略，所有实际时长都从当前 `NetworkSettings` 读取，不存在隐藏的硬编码超时或备用值。固定下载大小上限仍然生效。监控状态会按真实失败边界显示“元数据失败”“检查失败”“下载失败”或“安装失败”，底部状态栏和 Activity 同时保留原始错误，不会再把下载或安装错误统称为“获取失败”。

简洁用户配置示例：

```toml
[github.monitors.ripgrep]
repository = "BurntSushi/ripgrep"
asset = { product = "ripgrep", os = "macos", arch = "aarch64", format = "tar_gz" }
install = { type = "user_directory" }

[github.monitors.example]
repository = "owner/example"
asset = { product = "example", os = "macos", arch = "aarch64", format = "dmg" }
install = { type = "macos_application", application = "Example.app" }
update_policy = "automatic"
```

DMG 监控从目标 `.app/Contents/Info.plist` 读取 `CFBundleShortVersionString`，缺失时回退到 `CFBundleVersion`；应用不存在时显示未安装。其他格式使用 dvup 上次成功安装的 Release tag。停用项、探测失败项和已是最新版的项不会进入自动安装队列，且保存声明永远不会触发一次安装。

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

界面文案由 `assets/i18n/en.toml` 与 `assets/i18n/zh-CN.toml` 构建并嵌入可执行文件，不需要在运行目录旁分发语言包。新增或修改文案时，两份 TOML 必须保持相同的 key 和 `{0}`、`{1}` 编号占位符；`build.rs` 会在 `cargo build`、`cargo check` 和 `cargo test` 时校验语言目录，并从目录生成类型化消息 key。缺少 key、空值、占位符不一致或调用点拼错 key 都会直接阻止构建。运行期查询同样是严格的，不会静默回退成另一种语言。

在 TUI 的命令工具视图按 `c`，首先选择三条彼此独立的路径：

- “从包管理器添加”：选择 Homebrew、npm、pnpm、Cargo、pipx 或 uv，输入软件包后由对应官方 Registry 验证。dvup 结合结构化 Registry 元数据和本机包管理器的只读信息发现可执行文件；一个候选会自动填入，多个候选必须明确选择，没有候选时手动输入。随后只运行版本探测，默认参数为 `--version`，按 `Tab` 可在 `--version`、`version`、`-V` 间切换，也能输入自定义参数。探测必须成功并提取到版本；多个版本号必须由用户选择。确认页展示名称、manager、package、executable、当前版本、官方最新版本和确定性的更新方式。
- “添加自定义更新命令”：填写工具名称、更新命令和版本探测命令。命令按 argv 安全拆分，不通过 shell 拼接；dvup 只检查更新程序是否存在，绝不在向导中执行更新命令。版本探测会显示原始输出并要求得到唯一或人工确认的版本号。随后可选择“不比较，由工具自己处理”，或显式选择 npm、PyPI、crates.io、GitHub Release、GitHub Tag 之一并即时验证；验证失败不会自动切换来源。
- “使用 AI 分析命令工具”：AI 只提取包管理器、包名和候选名称；候选通过固定官方 Registry 验证后，进入与第一条完全相同的本地 executable 发现与 probe 流程。

所有 Registry、GitHub 和 probe 验证都在后台执行，不阻塞 TUI。每个请求都有唯一 ID；键盘输入、鼠标操作或粘贴改变身份字段后，迟到的旧结果会被丢弃，不能覆盖新输入。添加和编辑最终都进入同一套本地结构校验、摘要确认和原子保存管线。确认保存只更新 TOML 并刷新对应视图，不执行更新命令。

AI 是可选加速器，不是手动配置的依赖。在 Settings 页的 `AI 生成` 中配置 OpenAI 兼容的 Base URL、可选 API Key 和模型；`Ctrl+R` 获取模型，`Ctrl+T` 测试连接。Base URL 可填写到 `/v1`，dvup 会只追加一次 `/chat/completions`。远程服务必须使用 HTTPS，HTTP 仅允许 localhost/回环地址。API Key 使用与 GitHub 凭据相同的本机加密存储；关闭 AI 开关不会删除连接信息。

命令 AI 与 GitHub AI 使用两个严格、互不兼容的 JSON 协议。命令协议只接受 `name`、`manager`、`package`，GitHub 协议只接受 `name`、`repository`；未知字段、重复候选、Markdown、说明文字、安装脚本和错误类型都会被拒绝。命令请求返回仓库候选时只提示前往 GitHub 视图，GitHub 请求返回命令候选时只提示前往命令视图；不会自动切换视图、转换类型或修改配置分区。

AI 不再生成 probe、后台策略、进程规则、超时、重试、平台、资源组、Asset 正则、安装路径或安全上限，也不存在“第二阶段 AI 补全完整配置”。GitHub AI 只找到并验证仓库，随后进入与手动流程相同的真实 Asset 选择和语义 selector 回验。AI 关闭、未配置或服务不可用时，上述包管理器、自定义命令和 GitHub Release 手动流程仍然完整可用。

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

`dvup add <name> ...` 会创建 `type = "custom"` 声明，把更新命令保存为 `update = [...]`，并使用 `probe = ["<name>", "--version"]`。保存前必须实际运行该只读探针并提取到版本号；失败时命令会停止并提示使用 TUI 选择不同的 executable 或 probe 参数。更新命令本身不会在添加时执行。

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

推荐在命令工具视图按 `c`，选择“从包管理器添加”，再选择 npm 或 pnpm；这样会得到固定官方 Registry、确定性的更新命令和指定版本能力。下面的 `dvup add` 示例仍然有效，但它们按设计保存为自定义命令，适合脚本化场景。Bun 不属于这一版的六种包管理器模板，需要使用自定义命令。

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

普通 formula 推荐通过 TUI 的 Homebrew 包管理器向导添加，它固定使用 Homebrew Formula API 验证。需要 `--cask` 等特殊参数时使用自定义命令；自定义组合不会伪装成普通 formula 声明。

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
- 用户声明的资源组由包管理器模板或自定义更新程序本地推导，不再暴露底层 `resource_group` 字段。

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

按 `Shift+Tab` 切换策略。普通 `Tab` 在 Tools 页切换“命令工具 / GitHub 仓库”视图；包管理器向导的 probe 参数步骤中也可用 `Tab` 在常见参数间切换。切到 `TERMINATE` 时，策略会应用到当前状态目录中处于 Pending/Waiting 的任务：TUI 按 PID、进程名和启动时间停止精确匹配的进程，原 worker 随后继续原任务。切回 `WAIT` 只影响之后创建的新任务，不会撤销已经下达给后台任务的终止指令；已经处于 Running 的任务不会被策略切换打断。

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

普通用户可以完全通过 TUI 配置。需要严格编辑 TOML 时运行：

```console
dvup init
dvup
```

`dvup init` 在用户数据目录创建全局 `dvup_custom.toml`；模板位于 [configs/dvup.user.example.toml](configs/dvup.user.example.toml)。用户文件只有两个顶层分区：`commands` 和 `github.monitors`。二进制内置 starter manifest 仍使用完整运行时 `Config`，用户声明加载后编译成相同运行模型，并可覆盖同名内置命令；内置复杂配置不会暴露为用户表单字段。

包管理器命令：

```toml
[commands.codegraph]
type = "package"
manager = "npm"
package = "@colbymchenry/codegraph"
executable = "codegraph"
```

`probe_args = ["--version"]` 是默认值，只有工具使用其他参数时才写：

```toml
[commands.example]
type = "package"
manager = "cargo"
package = "example-cli"
executable = "example"
probe_args = ["version"]
```

自定义自更新命令：

```toml
[commands.deno]
type = "custom"
update = ["deno", "upgrade"]
probe = ["deno", "--version"]
```

需要比较官方最新版本时，可以增加一个显式来源：

```toml
[commands.deno]
type = "custom"
update = ["deno", "upgrade"]
probe = ["deno", "--version"]
latest = { provider = "github_release", repository = "denoland/deno" }
```

包管理器声明的更新命令、官方 latest source、指定版本能力、平台限制、等待进程、前后台策略、锁等待、重试和资源组全部由本地模板确定。自定义命令不会自动获得指定版本更新能力。

GitHub Release 监控：

```toml
[github.monitors.ripgrep]
repository = "BurntSushi/ripgrep"
asset = { product = "ripgrep", os = "macos", arch = "aarch64", format = "tar_gz" }
install = { type = "user_directory" }
```

DMG：

```toml
[github.monitors.example]
repository = "owner/example"
asset = { product = "example", os = "macos", arch = "aarch64", format = "dmg" }
install = { type = "macos_application", application = "Example.app" }
```

map key 就是命令或监控名称，因此同一分区不能重复；命令和 GitHub 分区可以使用相同名称而互不冲突。保存命令只修改 `commands`，保存 GitHub Monitor 只修改 `github.monitors`；一个分区为空时只省略该分区，两个分区都为空时删除空文件。写入使用同目录临时文件、flush/sync 和原子替换，失败时保留原文件。

schema 使用 `deny_unknown_fields` 严格验证。`[tools.*]`、`[[github.monitors]]` 数组、`asset_regex`、`target_directory`、`strip_components`、安全上限以及旧完整 `UserTool` 字段都直接报错；dvup 不探测旧版本、不迁移、不自动转换，也不提供 fallback。TOML 编辑器只接受这里的新结构。

显式传入用户清单时：

```console
dvup update --config path/to/dvup_custom.toml
```

此时仍以内置工具为基础，再应用指定的用户声明；不会同时加载全局 `dvup_custom.toml`，也不会扫描当前目录或父目录中的配置文件。

## 构建与验证

```console
cargo build --release
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

仓库 CI 会在 `ubuntu-latest`、`macos-latest` 和 `windows-latest` 上分别执行格式检查、Clippy、全部测试和 release 构建，确保三个平台的条件编译路径持续可用。

# quota-bar

轻量级 **Windows 任务栏 AI 套餐额度显示条**（Rust / 原生 Win32，单二进制，无运行时依赖）。

把各家 AI 编程套餐的**三窗口用量**（5小时 / 周 / 月已用百分比）和**金额类余额**（¥ / $ / credits）直接嵌在任务栏上：

```
● MiMo 63% · GO 3% · AGY 0%
```

点击弹出面板查看每家详情（三窗口 + 重置倒计时 + 余额 + 账号）；右键是完整菜单。

界面采用「石墨清单」：详情面板使用石墨灰实底、24 DIP 连续圆角、白色文字和独立 ClearType 字体；供应商与账号在左侧，额度、余额及其实际更新时间在右侧。面板设计宽度为 460 DIP、高度上限为 560 DIP，长清单通过滚动浏览。任务栏上的套餐不绘制容器框或图标底板，使用真实供应商图标、加粗文字及更醒目的进度色；状态栏不显示更新时间。全部 14 个供应商条目均内嵌图标，错误与等待状态也保留供应商标识。

任务栏内嵌模式保持透明；没有原生任务栏的屏幕使用统一石墨灰底的浮动额度条，并启用 ClearType，保证浅色窗口或壁纸上的文字清晰。浮动模式也不绘制单个套餐的边框。

面板底部可直接调整字号、刷新间隔及打开供应商菜单；顶部「刷新」立即请求更新。更新时间取自各账号的抓取记录，不随重绘改变；重置倒计时接在更新时间后面，与额度数值同行显示，节省竖向空间。长清单可用鼠标滚轮浏览，标题和控制区保持可见，面板不会遮挡打开它的额度条。点击面板内容不会关闭，点击外部或按 Esc 关闭。

## 特性

- **任务栏原生嵌入**：`WS_CHILD + SetParent(Shell_TrayWnd)`，不是悬浮窗、不是托盘图标；按托盘图标簇位置自适应宽度，**左右可停靠、永不遮挡托盘图标**；Explorer 崩溃 1 秒内自愈。
- **多屏**：每块显示器的任务栏各嵌一条（含 `Shell_SecondaryTrayWnd` 枚举），可在菜单切回「仅主屏」；Per-Monitor V2 DPI，混合缩放/负坐标副屏均正确。
- **低内存**：无 tokio / 无 GUI 框架，阻塞 HTTP + 单轮询线程 + GDI 绘制；`opt-level=z` + LTO + strip + 静态 CRT，常驻约 20–30 MB，空闲 CPU ≈ 0。
- **完全独立的凭证体系**：密钥存 Windows 凭据管理器（DPAPI 文件兜底），OAuth 自带登录，不依赖任何第三方工具的配置目录。
- **每家都能多账号**：同一个 provider 可添加任意多个账号（API key / Cookie 直接加；Antigravity / Codex 走浏览器登录），每个账号**各自独立抓取、独立成行**，可在菜单里**逐账号勾选是否显示在状态栏**。
- **菜单分工明确**：右键「**供应商管理**」只列**已添加**的供应商（点进去逐账号勾选 / 添加 / 移除）；「**添加供应商**」列出全部供应商（点开＝再加一个账号或登录）。没添加过的既不进管理列表也不上条。
- **过期套餐自动隐去**：上游明确告知套餐已过期（MiMo `tokenPlan/detail.expired`、StepFun `subscription.expired_at` 已过）时整行不上条/不进面板（`config.json` 的 `hide_expired_plans: false` 可关掉）。
- **凭据失效会提醒**：Cookie / 会话过期时弹一次托盘气泡（含更新指引），恢复后再次失效会重新提醒 —— 不用自己盯着才发现额度不更新了。
- **套餐 + 金额双模型**：缺失数值一律 `None`，绝不默认填 0（0% 是合法观测值，只能来自上游）。

## 支持的额度来源

| Provider | 套餐三窗口 | 金额余额 | 凭证 |
|---|---|---|---|
| 小米 MiMo（大陆） | ✅ | ✅（需平台 cookie） | 平台 Cookie（多账号） |
| 小米 MiMo（新加坡） | ✅ | — | 平台 Cookie（多账号） |
| OpenCode Go | ✅ | 视响应（useBalance 标志） | API key（多账号） |
| Google Antigravity | ✅ | ✅ credits（视响应） | **OAuth 登录（多账号）** |
| ChatGPT / Codex (OpenAI) | ✅ 5小时 / 周 | ✅ credits（视响应） | **OAuth 登录（多账号）** |
| 智谱 GLM / Z.AI | ✅ | ✅ credits | API key（多账号） |
| Kimi Code | ✅ | — | API key（月窗另需 cookie） |
| Command Code | ✅ | — | API key（多账号） |
| DeepSeek | — | ✅ 余额 | API key（多账号） |
| OpenRouter | ✅ | ✅ 余额 | API key（多账号） |
| 阶跃星辰 StepFun | ✅ 5小时 / 周 / 套餐 Credit 月池 | ✅ 余额（¥） | 平台会话 Cookie（多账号） |

> 首期核心为 MiMo / OpenCode Go / Antigravity（默认启用）；其余在配置中按需启用。
> Claude / Grok 预留于模型层（可录入凭据，但上游额度接口尚未接入）。

## 构建

```powershell
cargo build --release     # 需要 Rust 1.85+（edition 2024）与 MSVC 工具链
cargo test                # 290+ 单测，全部离线可跑（含真实开窗的输入框测试与真 HMENU 菜单结构回读）
```

产物：`target\release\quota-bar.exe`（静态 CRT，单文件，可直接拷走）。

## 使用

```text
quota-bar                    启动（任务栏内嵌条 + 托盘）
quota-bar json               抓一轮并打印 JSON（调试 / 与 dsh 插件比对）
quota-bar providers          列出全部 provider id（含凭据形态）
quota-bar setkey <id> <key>  设置 API key（覆盖为唯一账号并启用）
quota-bar setcookie <id> <cookie>        设置平台 Cookie（同上）
quota-bar addaccount <id> <key|cookie> [账号名]   追加账号（多账号）
quota-bar rmaccount <id> <账号id|前缀>             移除账号
quota-bar setkey|setcookie <id> --delete           删除该 provider 的全部账号
quota-bar enable <id> / disable <id>
quota-bar login <id>                   OAuth（antigravity / chatgpt）或网页登录（mimo / step）
quota-bar logout <id>                  OAuth 活动账号登出
quota-bar accounts <id>               列出账号（* = 活动账号）
quota-bar switch <id> <aid>           切换活动账号（OAuth 类）
quota-bar --help
```

示例：

```powershell
quota-bar setcookie mimo "<平台 cookie>"   # 小米 MiMo（平台接口只认 Cookie）
quota-bar addaccount mimo "<另一个号的 cookie>" "小号"   # 同一家多账号
quota-bar setkey opencode-go sk-xxxx       # OpenCode Go
quota-bar login antigravity                # 浏览器授权（回环 127.0.0.1:51121，支持粘贴回调兜底）
quota-bar login chatgpt                    # OpenAI/ChatGPT 授权（codex 为兼容别名）（回环 localhost:1455）
quota-bar accounts codex                   # 看已登录的账号
quota-bar enable deepseek                  # 启用 DeepSeek 余额
quota-bar                                  # 开跑
```

### 菜单（右键条 / 托盘图标）

```text
显示额度条 / 位置 / 多屏显示 / 字号 / 样式 / 轮询间隔 / 立即刷新
─────────────────────────────────────────────
供应商管理 ▸          只列**已添加**的供应商
    小米 MiMo ▸
        ✓ 在状态栏显示          （该供应商总开关）
        ✓ 主号 / ✓ 备用        （逐账号勾选：决定它上不上条）
        ＋ 添加账号…（Cookie）  （再添一个号：粘贴新 Cookie）
        移除账号 ▸ 主号 / 备用
    Google Antigravity ▸  ✓ 在状态栏显示 / ✓ 邮箱 ● / ＋ 登录新账号 / 移除账号 ▸
添加供应商 ▸          全部 13 家（✓ = 已添加；点开＝再加一个账号或登录）
─────────────────────────────────────────────
低额度气泡通知 / 开机自启 / 退出
```

### 环境变量（免配置兜底，keyring 未设时按序查找）

| Provider | 变量 |
|---|---|
| 小米 MiMo | `XIAOMI_MIMO_API_KEY` / `MIMO_API_KEY` / `MI_MIMO_API_KEY` / `XIAOMI_API_KEY` |
| 小米 MiMo SG | `XIAOMI_MIMO_SG_API_KEY` / `MIMO_SG_API_KEY` / … |
| OpenCode Go | `OPENCODE_GO_API_KEY` / `OPENCODE_API_KEY` |
| MiMo 平台余额（可选） | `MIMO_USAGE_COOKIE`（platform.xiaomimimo.com 的 Cookie，DevTools → Network → `/api/v1/balance`） |
| 阶跃星辰 StepFun | `STEPFUN_OASIS_TOKEN`（platform.stepfun.com 的 `Oasis-Token` cookie 值） |
| 智谱 / Z.AI | `GLM_API_KEY` / `ZHIPU_API_KEY` / `ZAI_API_KEY` …（`quota-bar providers` 查看） |
| DeepSeek / OpenRouter / Kimi / Command Code | `DEEPSEEK_API_KEY` / `OPENROUTER_API_KEY` / `KIMI_API_KEY` / `COMMAND_CODE_API_KEY` … |
| Kimi 月窗（可选） | `KIMI_AUTH_COOKIE`（`kimi-auth` cookie） |

## 配置与数据

- 配置：`%LOCALAPPDATA%\QuotaBar\config.json`（轮询间隔、左右停靠、多屏开关、字号、各 provider 开关与**账号表**、账号显示白名单、`hide_expired_plans`、低额度提醒阈值/重发周期、开机自启）
- 密钥/会话：Windows 凭据管理器（服务名 `quota-bar`），回退 `%LOCALAPPDATA%\QuotaBar\secrets\*.bin`（当前用户 DPAPI）；多账号的每个账号一条（`provider/<id>/api-key/<账号id>`、`provider/<id>/cookie/<账号id>`），OAuth 账号表在 `session/<id>`
- 账号显示白名单 `display_accounts`：键 = `"<provider>:<账号id>"`；`null` = 全部显示。旧格式（裸 id，只表达 Antigravity 意图）**自动兼容**——只作用于 AGY，菜单勾选一次即整份重算为新格式
- 日志：`%LOCALAPPDATA%\QuotaBar\debug.log`（1 MiB 轮转，发布版常开）

菜单改动即时生效（写盘 → 下一轮轮询热重载 → 重绘）；CLI 改动同样在运行中热生效。

## 低额度提醒

任一窗口「剩余 ≤ 阈值」（默认 10%）→ 托盘气泡告警；恢复后再次跌破立即重告，持续低迷按重发周期（默认 30 分钟）重复。阈值与周期在 `config.json`（`low_threshold_percent` / `resend_minutes`）。

## 架构与致谢

| 模块 | 来源 |
|---|---|
| 任务栏嵌入 / heal / 多屏扩展 / GDI 绘制 / 托盘 / 菜单 | 作者自己的 `D:\Project\blue\bt-battery-bar`（MIT） |
| 三窗口解析规则 / 各家端点 / OAuth（PKCE + 回环 + 刷新） | `dsh-plugin-subscriptions`（MIT，github.com/V1ki/dsh-plugin-subscriptions） |
| 数据模型（套餐/金额双轨、缺失不填 0）/ 凭据存储 / 智谱·DeepSeek·OpenRouter 解析 | 作者自己的 `D:\Project\win`（Quota Lens） |

线程模型：UI 线程（Win32 消息循环 + 1s 自愈心跳）× 轮询线程（`recv_timeout` 阻塞，到点/kick 触发，并行抓取写 `SnapshotStore` + 脏标记）× 每 provider 抓取线程（每轮瞬态）。登录流程跑在独立后台线程，结果 `PostMessage` 回主线程。

## 已知限制

- OpenCode `/zen/go/v1/usage` 与 MiMo `/v1/usage` 无官方样例，字段以 dsh 解析器接受集为准；余额字段（如有）解析不到时不显示，宁缺毋滥。
- **MiMo 平台会话是服务端短寿命 token**（`api-platform_serviceToken`）：过期后上游回 401 + loginUrl，客户端**无法自行续期**（无刷新端点，SSO 的 passport cookie 在 account.xiaomi.com 域下、跨域取不到）。条/面板会明确提示，按提示在「供应商管理 → 小米 MiMo → 添加账号…」重新粘贴新 Cookie 即可恢复。
- **StepFun 的额度只能走控制台会话**（`Oasis-Token`，API key 会被服务端 403 `api key not permitted for this method`）。会话段 30 分钟、设备段 30 天；程序在临近过期与 401 时各自动调一次 `RefreshToken`（无需密码）并把新 cookie 写回凭据管理器 —— 只要每半小时内跑过一轮，会话就能一直续下去；设备段满 30 天需重新登录粘贴。上游对"套餐不含的配额"返回 `0`（不是"已用完"），这类窗口整行隐藏，不会显示成 100%。
- Win11 副屏需开启「任务栏 → 在多个显示器上显示任务栏」才会存在 `Shell_SecondaryTrayWnd`；未开启时自动降级为仅主屏。
- Claude / Grok 尚未接入额度接口（可录入凭据，抓取侧给"尚未接入"提示）；「Cloud Code / Step」待确认具体产品后接入。
- 菜单里每个 provider 最多显示 6 个账号槽（超出用 `quota-bar accounts` 查看/管理）。

## ChatGPT 登录与 MiMo / Step 排查

- 右键任务栏额度条或托盘 → **添加供应商 → ChatGPT / Codex (OpenAI)（登录）**，在浏览器完成授权。也可执行 `quota-bar login chatgpt`；旧的 `login codex` 仍有效，沿用同一账号存储。
- 此项显示的是 ChatGPT 账号的 **Codex 订阅额度**（5 小时、周与 credits，以上游实际返回为准），不能据此推断 ChatGPT 网页聊天各模型的剩余消息数。登录后自动启用并加入当前账号显示列表。
- 若 Codex 菜单仍要求 API Key，正在运行的是旧版程序。退出旧实例后替换已安装的 `quota-bar.exe`，再启动新版。
- MiMo 和 Step 查询套餐需要平台会话 Cookie。只有 API Key 或仅勾选启用时，额度无法查询。右键 **添加供应商**，选择相应平台的「网页登录」，也可选「手动粘贴 Cookie」；已添加账号还需在 **供应商管理** 勾选显示。
- MiMo 手动配置：登录 [平台](https://platform.xiaomimimo.com)，F12 → Network → 选择平台 `/api/` 请求 → 复制 Cookie **请求头的值**。不要复制响应 Set-Cookie，也不要粘贴 API Key。会话失效时需重新登录并更新 Cookie。
- Step 手动配置：登录 [平台](https://platform.stepfun.com)，F12 → Application → Cookies → 复制完整 `Oasis-Token` 的值，也接受包含该字段的整条 Cookie。程序从令牌解析设备 ID，并尝试自动续期；身份已失效或设备段过期时需重新登录并更新令牌。认证重试最多一次，续期后的额度、套餐和余额查询共用新令牌。
- 可用 `quota-bar json` 检查 `error` / `message`。`no-cookie` 表示没有可读取的凭据；`auth` 表示会话失效；接口可达不代表账号授权有效。不要把 Cookie、令牌或完整凭据贴到日志或问题报告中。

## 网页登录与显示内容选择

MiMo / MiMo 新加坡 / Step 的「添加供应商」入口现在提供**网页登录**：程序打开专用 Edge（找不到时尝试 Chrome）登录窗口，你在窗口中完成平台登录后，程序自动取得会话并验证额度接口，再保存到 Windows 凭据管理器并启用账号。关闭窗口可以取消，五分钟未完成则超时。仅访问程序新建的专用浏览器配置，不读取你日常浏览器的登录资料。

此方式免去手动复制 Cookie，**底层仍是平台会话认证**。Step 官方 Step Code OAuth 返回模型调用凭据，目前未确认有对应的公开套餐额度查询接口，所以本版使用网页登录会话。会话失效时重新选择「添加账号…（网页登录）」；Step 在可续期时仍会自动刷新。已添加的供应商在管理菜单中保留「手动粘贴 Cookie…」备用入口，也仍支持 `setcookie` / `addaccount` 命令。命令行可执行 `quota-bar login mimo` 或 `quota-bar login step`。

右键 → **供应商管理 → 对应供应商 → 显示内容**，选择：

- **套餐额度和余额**：两种都显示（旧配置默认保持此行为）。
- **仅套餐额度**：显示套餐窗口，隐藏金额余额 / credits 余额。
- **仅余额**：显示金额 / credits 余额，隐藏套餐窗口与套餐档位。套餐过期不再隐藏这一余额行。

选择按供应商保存，对该供应商的所有账号生效，任务栏（紧凑图文 / 平铺数字）、详情面板和托盘悬浮提示一致，并立即刷新。原始接口数据与 `quota-bar json` 不受显示选择影响；平台未提供所选类别时显示「平台未返回套餐额度 / 余额」，不会伪造为 0。

配置保存在 `config.json` 的 `display_modes`，例如：

```json
"display_modes": {
  "codex": "plan",
  "mimo": "balance",
  "stepfun": "plan"
}
```

## 供应商排序与显示屏选择

供应商在每个显示器上由同一条额度条统一排列，间距由程序控制；不再保存任意坐标，也不再为每家供应商创建独立停靠窗口。旧的 `x_fraction` 配置只为兼容保留，显示时忽略。

- 拖到另一个供应商图标上：交换两家的顺序。
- 拖到另一屏的任务栏或额度条上：把该供应商切换到目标屏幕，自动加入该屏的排列。
- 右键 → **供应商管理 → 对应供应商 → 排序：向前 / 向后一位**，可直接调整排序。
- 同一菜单的 **显示屏** 可选择指定屏幕，或恢复「跟随多屏显示设置」。

普通单击仍打开详情，拖动超过距离阈值才开始；Esc 可取消。顺序与显示屏选择会保存，重启后保留。每家供应商的所有账号、套餐和余额保持在一起；空间不足时会显示 `+n`，供应商仍可通过菜单排序与切换显示屏。

副屏没有可用的 Windows 任务栏窗口时，程序会在该屏右下方提供固定的备用额度条。没有分配供应商的屏幕显示「拖入供应商」，可用于跨屏拖放。拔掉目标显示器时，供应商暂时回到可用屏幕，接回后恢复。主、副屏的显示开关仍可在右键「多屏显示」中选择。

右键 **恢复默认排序和显示屏** 可恢复全部布局；供应商管理中的 **恢复默认显示屏** 只清除该家的屏幕分配。

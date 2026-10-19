# 云同步功能：需求与设计

## 背景

用户在多台电脑间使用 SwitchHosts，每台都会积累一组 hosts 配置（不同项目各自的配置）。
换电脑重装后只能手动导出再导入，体验差。希望借免费的网络存储（代码托管平台仓库）保存
整库配置：新机器只需登录一次，就能自动拉取完成配置；日常本地改动自动推送到网络。

## 已确认的决策

| 决策点 | 选定方案 |
| --- | --- |
| 认证方式 | 个人访问令牌 PAT（两平台通用）+ GitHub 设备码授权（浏览器免密登录），任选其一 |
| 首版平台 | GitHub + Gitee（API 形态一致，一套实现覆盖） |
| 冲突策略 | 拉取覆盖本地、改动自动推送（最后推送者胜出）；覆盖前自动保留带时间戳的本地快照 |

Neon（网络 PostgreSQL）暂列入后续阶段：需引入 postgres 客户端依赖，包体积与复杂度成本高，
首版暂不做。

## 认证方式详述

设置页登录区提供"认证方式"选择：

1. **PAT（个人访问令牌）**：默认方式，GitHub / Gitee 通用。用户在平台生成令牌后粘入
   即完成"登录"。权限要求：能创建私有仓库 + 读写仓库内容（GitHub 的 `repo` scope；
   Gitee 的 `projects`、`user_info` scope）。
2. **GitHub 设备码授权**：仅 GitHub 提供此流程，Gitee 不支持。前端弹出 `user_code` 与
   `verification_uri`，用户在浏览器完成授权，后端轮询 `login/oauth/access_token` 直至拿到
   access token（等价于一张 PAT，生命周期等同）。

设备码流是 GitHub 官方流程，**不需要 client_secret，也不需要本地回调端口**，但要求一个
启用了 Device Flow 的 OAuth App 的 `client_id`。该 client_id 作为 GitHub provider 的可配置
项（默认留空）：未配置时该认证方式在 UI 中禁用并提示填入；填入后即可启用。GitHub 设备码
API 端点：

- `POST https://github.com/login/device/code`：入参 `client_id`、`scope`，返回 `device_code`、
  `user_code`、`verification_uri`、`expires_in`、`interval`（默认 5s）。
- `POST https://github.com/login/oauth/access_token`：轮询入参 `client_id`、`device_code`、
  `grant_type=urn:ietf:params:oauth:grant-type:device_code`；响应中 `authorization_pending`
  继续轮询、`slow_down` 加大间隔、`expired_token` / `access_denied` 终止。

设备码登录完成后，后端把拿到的 token 与 PAT 一视同仁地存入账户文件，后续同步请求路径
完全一致。

## 同步什么

完整复用现有 `switchhosts-backup` 格式（见 `src-tauri/src/import_export.rs::export_to_file`）：

- `manifest.root`：hosts 树（分组、标题、id、开关状态、远程节点元信息）
- `entries`：每个 hosts 节点的内容（按 id 索引）
- `trashcan.items`：回收站

整库打包为单个 JSON，作为远端仓库中的单个文件 `switchhosts-backup.json` 存储。优点：

1. 不发明新格式，导入导出代码可直接复用；
2. 远端 diff 简单（内容哈希比较），无变化不写；
3. 单文件，仓库干净，不依赖 git 历史直接用平台 Contents API 读写。

不参与同步的内容：`internal/config.json`（各机器偏好不同）、`state.json`（窗口/折叠状态）、
应用历史、DNS 缓存——这些是机器本地状态。

**重要：同步写回本地不走 preview/commit 的 Replace 路径**——那条路径会重新映射所有节点 id，
只适合一次性导入；同步必须**保留 id 原地覆盖**，才能反复同步稳定，并保住远端节点与本地
hosts 文件的对应关系。拉取覆盖由云同步模块自写：解析载荷 → 替换 manifest.root 与 entries →
清掉老节点对应的内容文件 → 在 `storage::transaction::run` 内一次性落盘（复用 undo journal，
断电可回滚）。

## 账户与凭证存储

- 文件：`~/.SwitchHosts/internal/sync_account.json`，权限 600。
- 字段：

| 字段 | 说明 |
| --- | --- |
| `platform` | `github` / `gitee` |
| `auth_method` | `pat` / `device`（仅 GitHub 可为 `device`） |
| `login` | 用户名，验证令牌时从平台 `/user` 获取 |
| `repo` | 仓库名 |
| `token` | PAT 或设备码换得的 access token |
| `device_client_id` | 设备码登录用的 OAuth App client id（仅 auth_method=device 时存在） |
| `last_push_at` / `last_pull_at` | 最近同步时间（ISO 字符串） |
| `last_hash` | 上次同步时载荷的内容哈希，用于判断"本地是否又变了"与"远端是否与本地一致" |

安全边界（已知取舍，写明）：

- token 以明文存于本地文件，仅受文件权限保护；未引入系统钥匙串。
  后续可迁移到 macOS Keychain / Windows Credential Manager（`keyring` crate）。
- 所有请求仅发往所选平台的官方 API 域名；令牌不会写入推送内容、日志或事件载荷。
- 设备码轮询在前端/后端事件驱动下完成，有最大等待时长，终止即终止，无后台泄漏。

## 平台 API 映射

| 步骤 | GitHub | Gitee |
| --- | --- | --- |
| 验证令牌 | `GET /user`（Authorization: Bearer） | `GET /api/v5/user`（param access_token） |
| 查仓库 | `GET /repos/{owner}/{repo}` | `GET /api/v5/repos/{owner}/{repo}` |
| 建仓库 | `POST /user/repos`（private=true） | `POST /api/v5/user/repos`（private=true） |
| 读文件 | `GET /repos/{owner}/{repo}/contents/switchhosts-backup.json` | `GET /api/v5/repos/{owner}/{repo}/contents/switchhosts-backup.json` |
| 写文件 | `PUT /repos/{owner}/{repo}/contents/...`（base64 + sha） | `PUT /api/v5/repos/{owner}/{repo}/contents/...`（base64 + sha） |

差异收敛在 provider 适配层，上层流程代码对平台无感：token 注入方式（header vs query param）、
响应字段名、建仓库私有参数命名。

## 流程

### 首次向导（登录 → 自动配置）

设置页"云同步"页未登录时呈现向导，共三步，与用户描述一一对应：

1. 配置认证：选平台（GitHub / Gitee）+ 选认证方式（PAT 或浏览器授权——后者仅在
   GitHub 且 client_id 已配置时可用）→ 验证/token 获取，失败则提示原因；成功拿到
   登录名，进入下一步。
2. 输入仓库名（默认 `switchhosts-backup`，可改）→ 点"下一步"执行检测与初始化：
   - 仓库不存在 → 创建私有仓库 → 推送本地数据 → 完成（本地为源）；
   - 仓库存在且含配置文件 → 拉取并覆盖本地（覆盖前保存本地快照）→ 完成（远端为源）；
   - 仓库存在但无配置文件 → 推送本地数据 → 完成（本地为源）。
3. 展示结果摘要（建了仓库/覆盖了本地/推送了本地，条目数）并保存账户信息。

### 启动静默拉取

`lib.rs::setup` 中：应用启动且数据目录可用时，若存在账户信息且非数据目录恢复状态，
延迟数秒后静默拉取一次（避免阻塞启动），流程：

1. 读远端配置文件；
2. 与本地 `last_hash` 比较，不一致才继续（一致则零成本跳过）；
3. 覆盖本地前，先写快照到 `~/.SwitchHosts/internal/snapshots/<时间戳>.json`（保留最近 N 份）；
4. 解析并原子落盘（复用 manifest/entries/trashcan 写入原语，在 `lock_store` 内完成）；
5. 广播 `reload_list` 让前端刷新树；失败仅记日志与状态，不打断使用。

### 本地改动自动推送

在数据变更类 Tauri 命令成功路径（`set_list`、`set_hosts_content`、`move_to_trashcan`、
`move_many_to_trashcan`、`restore_item_from_trashcan`、`clear_trashcan`、
`delete_item_from_trashcan`、`commit_import`）末尾调用 `cloud_sync::on_data_mutated(app)`：

- 标记"本地有变更"并启动防抖定时器（~3s），期间多次触发只推一次；
- 到期后若已登录：构建备份载荷 → 与 `last_hash` 比较 → 不同则 PUT 到远端，
  成功更新 `last_push_at`/`last_hash`，失败记日志下次再试（不阻塞用户操作）。

不挂钩 `hosts_apply`（写系统 hosts 文件是本机动作，不代表配置库变化）。

### 手动同步

已登录状态页提供：立即拉取（覆盖本地+快照）、立即推送、登出（清空账户文件）。

## 冲突与安全

- 策略即用户确认的"覆盖 + 快照保底"：两台机器交叉修改时，最后推送方胜出；
  被覆盖方可在 `internal/snapshots/` 找回时间戳快照手动恢复。
- 拉取覆盖前对载荷做基本校验：格式标识、schema 版本、根节点可解析；校验失败拒绝覆盖并保留本地。
- 所有远端写操作都发生在 `lock_store` 之外（网络慢不应阻塞读写），仅本地落盘阶段持锁。

## UI

偏好设置抽屉新增"云同步"Tab（`src/renderer/components/Pref/CloudSync.tsx`）：

- 未登录：平台 Select + 认证方式 Select + 令牌输入 / 设备码登录（`user_code` 展示+轮询等待）
  + "验证" → 仓库名输入（默认值）→ "下一步" → 检测结果摘要 + 完成。
- 已登录：账户信息（平台、认证方式、登录名、仓库）、上次同步时间、手动"立即拉取/立即推送"、登出。
- 同步过程用 Mantine Notifications 反馈，静默拉取失败不弹打扰式通知。

i18n：至少补齐 `zh` 与 `en` 两套文案。

## 实现清单

Rust（`src-tauri/src/cloud_sync/`）：

- `mod.rs`：模块入口、全局同步状态（防抖、进行中标志）。
- `provider.rs`：`Provider` trait + `GitHub`/`Gitee` 实现（verify/has_repo/create_repo/get_file/
  put_file，GitHub 额外 `start_device_code` / `poll_device_token`）。
- `account.rs`：账户文件读写（原子写、600 权限）。
- `payload.rs`：内存中构建备份 JSON、内容哈希、本地快照写入、保 id 覆盖落盘。
- `engine.rs`：首次初始化 `ensure_setup`、`pull_overwrite`、`push_if_changed`、`on_data_mutated` 防抖。
- `commands.rs` 中新增命令：`sync_status`、`sync_verify_token`、`sync_start_device_login`、
  `sync_poll_device_login`、`sync_setup`、`sync_pull`、`sync_push`、`sync_logout`，
  其中设备码命令仅平台为 GitHub 且 `device_client_id` 非空时有效。
- 注册到 `lib.rs` 的 handler；`lib.rs::setup` 挂启动静默拉取；数据命令挂 `on_data_mutated`。

前端：

- `Pref/CloudSync.tsx` 向导与状态页；`Pref/index.tsx` 加 Tab。
- `agent.ts` Actions 增加同步方法；`zh.ts`/`en.ts` 文案。

## 测试

- Rust 单测：provider 请求构造/响应解析（离线，构造伪 JSON）、payload 构建与哈希、
  账户文件读写、快照轮转、解析校验拒绝坏载荷、设备码轮询状态机（pending/slow_down/
  expired/denied 转换）。
- `cargo test --locked --manifest-path src-tauri/Cargo.toml`
- 手工验证（需用户真实 PAT / 设备码 client_id）：GitHub PAT 建仓库→推送；
  GitHub 设备码登录→推送；第二台机器/清空数据目录后登录→拉取覆盖；本地增删改条目→自动推送；
  拔网线触发失败不崩溃。

# 更新记录

## [Unreleased] 0.4.1

下一个发布版本，尚未发布。内容是配置界面分页，以及teldrive 自身版本的检查与升级；旧控件全部保留，`key` 没有改动，所以已经填过的配置在升级后继续有效。

**分页**

- `config.json` 增加 `plugin_setting.pages`：部署 / Telegram 登录 / 频道与机器人 / 远程挂载 / 维护，每个控件多一个 `page` 字段。
- 状态行、进度、校验表保持公共（不挂 `page`），在任何页都看得到。
- `line` 相同的按钮仍在同一页：宿主只合并相邻且同页的控件，跨页会被拆行。
- 切页不触发插件动作，表单内容也不会因为翻页丢掉。
- 宿主不认识 `pages` 时忽略 `page`，所有控件平铺一页，功能不变。
- 去掉 ①…⑦ 编号与多余分隔线，中英文案同步重写；补一条「前置条件」提示，说清 teldrive 后端来自 tgdrive 的 rclone fork。

**检查与升级**

- 维护页显示当前版本与最新版本，两个动作在同一行；升级不可用时按钮直接灰掉，不让人点了才失败。
- `check_update` 读 `api.github.com/repos/tgdrive/teldrive/releases/latest`，按本机架构挑 `-windows-amd64.zip` / `-arm64.zip`，再取同一次 release 的 `teldrive_checksums.txt` 记下 sha256。只读，不动任何文件。
- `update` 一次做完：下载 → 按校验文件核安装包的 sha256 → 解压找 `teldrive.exe` → 查 PE 与架构 → 复制成 `teldrive.new.exe` 并核对它与解包出来的那个 exe 逐字节一致 → 备份 `teldrive.exe.bak` 并记下它的 sha256 → 停本插件启动的进程 → 覆盖 → 用原参数重启 → 等 `/health/ready`。
- 校验失败一律中止，不做「跳过」：安装包 hash 对不上校验文件、解包 exe 与暂存 exe 不一致、拿不到 sha256、读不出 PE 架构、架构与 `env.info.arch` 不匹配。
- 只停本插件这一次启动的进程：`proc.status` 认得那个句柄而且还在跑，才算自己的。端口通着但拿不到句柄时拒绝升级，并请用户手动停掉原进程；不按历史 pid 去杀（pid 会被系统复用）。
- 健康检查在一次调用内有界轮询：最多 20 次、间隔 1.5 秒、单次请求 4 秒超时，不用启动后立刻打一次 HTTP 就判失败。整条升级 13 轮左右，回滚分支也在宿主轮数上限内（默认 16 轮，`update` 这类动作 32 轮）。
- 升级前是停着的，健康检查完就停回去；不会顺手把服务拉起来。停回去失败时状态是黄色并说明没能恢复原来的运行状态，进程句柄留在 kv 里，否则用户点「停止」会被当成外来进程拒绝。
- 失败回滚分步执行、逐步看结果：先停新进程，再把旧 exe 复制回去，然后用原参数重启。`proc.stop` 回 `stopped:false` 也算失败，不会接着去覆盖一个还在运行的 exe；复制回去的 exe 要跟备份的 sha256 一致，不一致就报出两个哈希。任何一步失败都照实说明「没恢复成功、需要手动处理」。
- 新进程起来但宿主没给句柄时，既不谎称成功也不回滚：此时 exe 已是新版，回滚会再起一份旧进程抢同一个端口。状态直接说明拿不到句柄、请手动检查进程，kv 不做任何写入。
- 升级不需要先点「检查新版本」：它自己下载校验文件核对。
- `pgdata\`、`config.yaml`、session、机器人 token 全程不碰，也不复用「一键部署」那条会重写配置的路。

**建远程前先探后端**

- `on_remote_create` 先用 `rclone.call` 打 `config/providers`，确认当前连着的 rclone 有 `teldrive` 后端再建。缺后端时面板上给三条做法（用本插件部署、把主程序用的 rclone 路径指向已有部署的 `bin\rclone.exe`、自己起 fork 让 RCGUI 连上），不会硬建然后回一句 `unknown backend`，也不会另起第二个内核。

**工程**

- 22 个测试全部通过真实的 `dispatch` 驱动状态机：成功全流程、坏包校验不碰 exe、暂存校验不符不停服务、空架构必失败、无句柄必拒绝、复制失败回滚、健康检查失败回滚、原本停止的服务保持停止、计划跨步不丢、回滚重启后句柄写回且能再升级一次、停不掉不覆盖 exe、恢复后哈希不符要报错、空句柄不回滚。
- `.github/workflows/ci.yml` 跑 `cargo fmt --check`、`cargo test`、`cargo check --target wasm32-wasip1`，并校验 `config.json` 结构（page 声明、line 不跨页、key 不重复）与中英文案键值一致。
- `release.yml` 只在打 tag 时发布，打包前先校验 tag / `config.json` / `Cargo.toml` / `Cargo.lock` 版本一致并跑格式、测试、wasm 编译；同名 Release 已存在就失败退出，不覆盖已发布资产的 sha256。手动触发只产出 artifact。
- 打包脚本给编译加了 `--remap-path-prefix`：发布的 wasm 里不再出现本机的 cargo 路径。

## 0.3.0

- 登录、频道、机器人、远程挂载拆成独立分组；便携 PostgreSQL 支持自动/已有/便携/自动四种模式。
- 部署流程加上 PE 架构校验、健康检查重试与失败提示。

## 0.2.0

- 首次可用版本：一键部署 teldrive + rclone fork、扫码或验证码登录、建远程并挂载盘符。

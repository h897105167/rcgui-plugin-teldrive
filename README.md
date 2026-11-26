# rcgui-plugin-teldrive

RCGUI 插件：在本机一键部署 [teldrive](https://github.com/tgdrive/teldrive) 服务器，登录 Telegram、建频道与机器人、建 teldrive 远程，最后把远程挂成 Windows 盘符。装好之后还能检查并原地升级 teldrive 本身。

## 需要什么

- Windows 10 1803 或更新（便携数据库解压要用系统自带的 `tar.exe`）。
- RCGUI 能加载 WASI 插件（`plugins/manifest.json` 里有条目）。面板分页要宿主 0.3.0 或更新；更老的宿主会忽略分页字段，控件平铺一页，功能不变。
- 一个 Telegram 账号，能收到验证码。
- 挂载那一步要求 RCGUI 当前连接的 rclone 带 `teldrive` 后端 —— 原版 rclone 没有，得用 tgdrive 的 fork。插件在创建远程前会先向 rclone 要一次后端列表，缺后端时不会硬建，而是把三条可选做法写在面板上。

下面写的是 0.4.1 发布后的样子。**这一版还没有发布**：已发布的版本是 0.3.0、0.2.0，`plugins/manifest.json` 里现在填的也还是发布过的那个版本；0.4.1 要等这里的 Release 建好之后才能引用。

不用申请 `api_id` / `api_hash`：teldrive 的 `telegram.app-id` / `app-hash` 是写死在 `src/cfg.rs` 里的常量（跟官方示例配置一致），要换就改那两行。

## 面板分页

宿主 0.3.0 起才认 `plugin_setting.pages`；本插件用它把控件分到五页，切页不触发任何插件动作，表单里填的东西也不会丢：

| 页 | 里面有什么 |
| --- | --- |
| 部署 / Deployment | 数据目录、HTTP 端口、数据库方式与连接串、一键部署、启动/停止 |
| Telegram 登录 / Telegram sign-in | 扫码登录、验证码登录、两步验证密码、退出登录 |
| 频道与机器人 / Channel and bots | 建频道、选频道、加机器人 token、选/删机器人 |
| 远程挂载 / Remote and mount | 远程名、盘符、创建远程、挂载、卸载、传输代理 |
| 维护 / Maintenance | 当前版本、最新版本、检查新版本、升级 teldrive |

顶部那行状态、进度条、校验表是公共控件，故意不挂 page，所以在任何页都看得到。同一行的按钮始终在同一页（`line` 不跨页），否则宿主会把一行拆成两行。

宿主还不认识 `pages` 时（0.3.0 以前），这些 `page` 字段会被忽略，所有控件平铺在一页，功能不受影响。

## 装上它

```powershell
cd scripts
pwsh ./pack.ps1          # 构建 → dist/teldrive-0.4.1.zip，并打印 zip 的 sha256
```

把打印出来的 id / 版本 / sha256 抄进本体仓库的 `plugins/manifest.json`（0.4.1 的 Release 还没建，下面这份等发版之后再用，`sha256` 用打包脚本打印的那串）：

```json
{
  "id": "teldrive",
  "name": "teldrive",
  "repo": "https://github.com/h897105167/rcgui-plugin-teldrive",
  "tag": "v0.4.1",
  "asset": "teldrive-0.4.1.zip",
  "sha256": "打包脚本打印的那串",
  "dir": "teldrive"
}
```

然后在本体里跑 `scripts/fetch_plugins.ps1` 拉包，重启 RCGUI，插件面板里就会出现「teldrive」。

## 第一次跑

1. **部署页**：数据目录（默认在 RCGUI 数据目录下）、HTTP 端口 8080、数据库端口 5432。
2. **数据库**：默认「便携数据库」；手上已经有 PostgreSQL（本机、服务器或云数据库）就选「已有数据库」并把连接串贴上去，一个字节都不用下载；「自动」按本机 5432 → docker → 便携版这个顺序挑。
3. 点 **一键部署**：下载 teldrive、tgdrive 的 rclone fork（便携库模式再加 PostgreSQL 17.6）→ 解压（便携库是 jar 里的 txz，再解一层）→ 生成签名密钥与数据密钥 → 写 `config.yaml` → 启动 → 轮询 `/health/ready`，最多重试 3 次。
4. **Telegram 登录页**：填国际格式手机号（`+8613800138000`）→ 发送验证码 → 提交验证码；开了两步验证的账号会再要一次密码。登录成功后插件顺手建一个 API key，它的 secret 就是后面远程用的 `api_key`。
5. **频道与机器人页**：频道名留空就用默认名；机器人 token 要先在 [@BotFather](https://t.me/BotFather) 里 `/newbot` 建好，每行一个粘进来。
6. **远程挂载页**：远程名默认 `teldrive`，盘符默认 `T:`（填 `t`、`T:\`、`t:/` 都行）→ 创建远程 → 挂载。

## 升级 teldrive

维护页的「检查新版本」只读，不碰任何文件：它去 `api.github.com/repos/tgdrive/teldrive/releases/latest` 取最新 tag，按本机架构挑 `teldrive-<版本>-windows-amd64.zip` 或 `-arm64.zip`，再下载同一次 release 里的 `teldrive_checksums.txt`，比对里面的 sha256。「升级 teldrive」才会真的动手，顺序是：

1. 再查一次 release，比版本；不比当前新就什么都不做。
2. 下载 zip 到 `<数据目录>\tmp\`，用宿主下载时算出的 sha256 跟校验文件里那一行比对，不一致就删掉下载物并停下。这一步不会停服务，也不会碰 exe。
3. 解压到 `<数据目录>\tmp\unpack\`，找出 `teldrive.exe`，记下它的 sha256，检查 PE 头与架构（读不出架构就中止，不猜）。
4. 复制成 `bin\teldrive.new.exe`，再算一次它的 sha256，必须与第 3 步那个相同。这里比的是「解包出来的」和「复制到位的」两个 exe；安装包的 sha256 只在第 2 步跟校验文件比。
5. 把当前的 `bin\teldrive.exe` 备份成 `bin\teldrive.exe.bak`。
6. 停掉**本插件这一次启动的** teldrive：宿主 `proc.status` 认得句柄并且还在跑才算自己的。句柄拿不到就直接拒绝，见下面第二条硬规矩。
7. 把 `teldrive.new.exe` 覆盖到 `bin\teldrive.exe`，用原来的参数（`run --config <数据目录>\config.yaml`）重新启动。启动后先问一次 `proc.status`，确认这个句柄对应的进程还活着；宿主没给句柄就直接报错说明现状，且不回滚 —— 那时 exe 已是新版，回滚会再起一份旧进程抢同一个端口。
8. 在一次调用里轮询 `http://127.0.0.1:<端口>/health/ready`：最多 20 次、每次间隔 1.5 秒、单次请求 4 秒超时。200 才算成功，然后清掉 zip 与解压目录。
9. 升级前是停着的，健康检查通过后再停回去，并告诉你"已升级、服务仍按原来的停止状态"。

任何一步失败都会拆开做回滚：先停掉这次启动的进程，再把 `.bak` 复制回 `bin\teldrive.exe`，最后按升级前的状态决定要不要用原参数重启。回滚的每一步都看结果：`proc.stop` 回 `stopped:false` 也算没停成功，那就不会去覆盖一个还在运行的 exe；复制回去之后要重算 sha256，跟备份对不上就报「备份是 A、恢复后是 B」。哪一步没成功就照实说"没恢复成功、需要手动处理"，不会只写一句"已回滚"。真的重启成功了，新进程的句柄会写回 kv —— 否则用户下次点停止，插件认不出这个进程。`pgdata\`、`config.yaml`、session、机器人 token 全程不碰。

三条硬规矩：

- 上游 release 里没有 `teldrive_checksums.txt`，或者校验文件里没有这个 zip 的那一行：不安装，面板上直接说“拿不到可信校验值”。宿主下载器返回的 sha256 与校验文件不一致时同样不安装。任何一环拿不到 sha256（包括解包后的 exe）都算失败，不当成"跳过"。
- 端口通着但宿主不认识本插件的进程句柄：说明这个 teldrive 是别人（或你手动）起的，插件拒绝替换，请你自己先停。插件不会按历史 pid 去杀进程（pid 会被系统复用，杀错就是别人的进程）；确实要用 pid 时，先用 `Get-CimInstance` 核对那个 pid 的映像路径就是本插件的 `teldrive.exe` 才动手。
- 只替换 `bin\teldrive.exe` 这一个文件，不复用「一键部署」那条路 —— 那条路会重写 `config.yaml` 并重新初始化数据库。

`fs.rename` 在宿主里不存在，所以替换实际是「复制覆盖 + 复核 sha256」，不是原子 rename。覆盖前已经停服并做了备份，最坏情况是被占用失败，那时插件会回滚并说明。整条升级链 13 轮宿主调用，连回滚分支也在轮数上限内（默认 16 轮，`update` 这类动作 32 轮）。

## 权限

`config.json` 声明的能力：`kv` `fs` `http` `proc` `net` `rclone` `env`。实际用到的操作：

| 能力 | 用来做什么 |
| --- | --- |
| `kv` | 记住部署位置、端口、数据库方式、登录态、频道/机器人选择、挂载点、升级检查结果 |
| `fs` | 建目录、解压、复制（部署与升级的备份/替换）、读校验文件、删临时文件、打开数据目录 |
| `http` | 官方 release API、校验文件与 zip 下载、本机 `127.0.0.1` 的 teldrive API 与健康检查 |
| `proc` | 起/停 teldrive 与便携 PostgreSQL、`tar.exe` 解压、`taskkill` 收尾、`ping` 当 sleep |
| `net` | 探测端口是否已被占用（判断服务是否在跑、判断是不是别人起的） |
| `rclone` | 建远程、挂载、卸载，以及创建前查询后端列表 |
| `env` | 取数据目录、架构、是否提权 |

升级只从 `github.com/tgdrive/teldrive` 与 `api.github.com` 取文件，不需要额外能力，也不会把安装命令交给 shell 执行。

## 限制与失败时的行为

- 挂载、卸载、建远程都通过 `rclone.call` 打到 **RCGUI 当前连着的那个 RC 端点**，插件不自己起 rclone 进程。那个 rclone 不是 fork 时，插件在创建远程前就会提示缺后端，并给出三条做法：用本插件部署（自带 fork）、把主程序用的 rclone 路径指向已有部署里的 `bin\rclone.exe`、或者自己起一个带 teldrive 的 rclone 让 RCGUI 连上。插件不会为了这件事再起第二个内核。
- 便携 PostgreSQL 是从 Maven Central 下的 22 MB 精简包（只有 `initdb`/`pg_ctl`/`postgres` 三个程序，没有 `psql`），解压后约 97 MB；建库走 `postgres --single` 单用户模式。它以非管理员身份运行，提权环境下会直接拒绝启动。
- 数据库方式选「已有数据库」时只要求填连接串，插件不会去探测远端端口，连不通由健康检查报出来（日志里会有驱动的错误原文）。
- 频道与机器人依赖 Telegram 侧的状态：频道名重复、token 无效都由 teldrive 的服务端错误说了算，插件只转达。
- 升级只跟踪官方 release 的最新版，不做降级，也不支持测试版（prerelease）。
- 只支持 Windows；不负责申请 `api_id`/`api_hash`，也不改你的 rclone 全局配置之外的东西。

## 开发

```powershell
cargo fmt
cargo test
cargo check --target wasm32-wasip1
cargo build --release --target wasm32-wasip1   # → target/wasm32-wasip1/release/main.wasm，约 300 KB
pwsh scripts/pack.ps1 -SkipBuild               # 只重新打包，不重新编译
```

插件是 WASI 程序：宿主把请求 JSON 写进 stdin，插件把应答 JSON 写到 stdout。一次操作可以要求宿主先做若干件事（读 kv、下载、运行命令…），宿主把结果回填后再调一次，默认最多 16 轮（`update` 这类动作 32 轮）。协议细节见本体的 `docs/PLUGIN.md`。

不开 RCGUI 也能验：本机有 Node（≥ 20）就够，用 Node 的 WASI 直接跑 wasm，再用一个几十行的脚本冒充宿主，把每轮 need 的能力用假数据答回去。真实宿主只在最后一步（真下载、真正起进程）才有区别，所以「状态机有没有走对、文案对不对、按钮亮不亮」这类问题都能这样查出来。缺省表单放 `form-<action>.json`，一轮里写回的 kv 由脚本自己累积，相当于宿主的持久化。

```
src/main.rs    WASI 入口、manifest、按 action 分发
src/deploy.rs 一键部署状态机（prep → dl → unzip → find → secret → cfg → run → health）
src/update.rs 检查与原地升级（下载 → 校验 → 备份 → 停服 → 替换 → 起服 → 有界轮询健康检查，必要时逐步回滚）
src/svc.rs    启停服务、面板刷新
src/store.rs  Telegram 登录、建频道、加机器人
src/rc.rs     建 remote、挂载、卸载，创建前的后端探测
src/api.rs    teldrive HTTP 调用与鉴权头
src/cfg.rs    config.yaml 与连接串拼装
src/paths.rs  数据目录布局
src/proto.rs  need/ops 应答与 kv 读写
src/i18n.rs   zh/en 文案
```

## 许可

MIT，见 [LICENSE](LICENSE)。

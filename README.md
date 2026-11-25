# rcgui-plugin-teldrive

RCGUI 插件：在本机一键部署 [teldrive](https://github.com/tgdrive/teldrive) 服务器，登录 Telegram、建频道与机器人、建 teldrive 远程，最后把远程挂成 Windows 盘符。

## 需要什么

- Windows，RCGUI 已装好并支持插件（`plugins/manifest.json`）。
- 一个 Telegram 账号，能收到验证码。
- 挂载那一步要求 RCGUI 当前连接的 rclone 带 `teldrive` 后端 —— 原版没有，必须用插件部署时放下的那个 fork。

不用申请 `api_id` / `api_hash`：teldrive 的 `telegram.app-id` / `app-hash` 是写死在 `src/cfg.rs` 里的常量（跟官方示例配置一致），要换就改那两行。

## 装上它

```powershell
cd scripts
pwsh ./pack.ps1          # 构建 → dist/teldrive-0.1.0.zip，并打印 zip 的 sha256
```

把打印出来的 id / 版本 / sha256 抄进本体仓库的 `plugins/manifest.json`：

```json
{
  "id": "teldrive",
  "name": "teldrive",
  "repo": "https://github.com/h897105167/rcgui-plugin-teldrive",
  "tag": "v0.1.0",
  "asset": "teldrive-0.1.0.zip",
  "sha256": "打包脚本打印的那串",
  "dir": "teldrive"
}
```

然后在本体里跑 `scripts/fetch_plugins.ps1` 拉包，重启 RCGUI，插件面板里就会出现「teldrive」。

## 第一次跑

1. **① 部署位置**：数据目录（默认在 RCGUI 数据目录下）、HTTP 端口 8080、数据库端口 5432。
2. **② 数据库**：默认「已有数据库」—— 手上已经有 PostgreSQL（本机、服务器或云数据库）就把连接串贴上去，一个字节都不用下载。确实没有才选「便携数据库」（下载 22 MB 自带一份）或「自动」（自动会按本机 5432 → docker → 便携版这个顺序挑）。
3. 点 **一键部署**：下载 teldrive 2.0.14、rclone fork v1.76.0（便携库模式再加 PostgreSQL 17.6）→ 解压（便携库是 jar 里的 txz，再解一层）→ 生成签名密钥与数据密钥 → 写 `config.yaml` → 启动 → 轮询 `/health/ready`，最多重试 3 次。
4. **③ 服务**：启动 / 停止 teldrive 服务器；「打开目录」跳到数据目录。
5. **④ Telegram 登录**：填国际格式手机号（`+8613800138000`）→ 发送验证码 → 提交验证码；开了两步验证的账号会再要一次密码。登录成功后插件顺手建一个 API key，它的 secret 就是后面 remote 用的 `api_key`。
6. **⑤ 频道与机器人**：频道名留空就用默认名；机器人 token 要先在 [@BotFather](https://t.me/BotFather) 里 `/newbot` 建好，每行一个粘进来。
7. **⑥ 远程与挂载**：远程名默认 `teldrive`，盘符默认 `T:`（填 `t`、`T:\`、`t:/` 都行）→ 创建远程 → 挂载。

## 限制与失败时的行为

- 挂载、卸载、建远程都通过 `rclone.call` 打到 **RCGUI 当前连着的那个 RC 端点**，插件不自己起 rclone 进程。那个 rclone 不是 fork 时，rclone 会回 `unknown backend "teldrive"`，插件把这句原样显示 —— 换成 `<数据目录>\bin\rclone.exe`（部署时放下的 fork）带 `--rc` 启动再连即可。
- 便携 PostgreSQL 是从 Maven Central 下的 22 MB 精简包（只有 `initdb`/`pg_ctl`/`postgres` 三个程序，没有 `psql`），解压后约 97 MB；建库走 `postgres --single` 单用户模式。解压那一步要用系统的 `tar.exe`（Windows 10 1803 起自带）。它以非管理员身份运行，提权环境下会直接拒绝启动。
- 数据库方式选「已有数据库」时只要求填连接串，本机或云端的 PostgreSQL 都行；插件不会去探测远端端口，连不通由健康检查报出来（日志里会有驱动的错误原文）。
- 版本是写死的常量（`src/deploy.rs` 的 `TELDRIVE_VER` 与两条下载地址），升级要改代码重新打包。
- 频道与机器人依赖 Telegram 侧的状态：频道名重复、token 无效都由 teldrive 的服务端错误说了算，插件只转达。
- 只支持 Windows；不负责申请 `api_id`/`api_hash`，也不改你的 rclone 全局配置之外的东西。

## 开发

```powershell
cargo build --release --target wasm32-wasip1   # → target/wasm32-wasip1/release/main.wasm，约 290 KB
pwsh scripts/pack.ps1 -SkipBuild               # 只重新打包，不重新编译
```

插件是 WASI 程序：宿主把请求 JSON 写进 stdin，插件把应答 JSON 写到 stdout。一次操作可以要求宿主先做若干件事（读 kv、下载、运行命令…），宿主把结果回填后再调一次，最多 16 轮。协议细节见本体的 `docs/PLUGIN.md`。

不开 RCGUI 也能验：本机有 Node（≥ 20）就够，用 Node 的 WASI 直接跑 wasm，再用一个几十行的脚本冒充宿主，把每轮 need 的能力用假数据答回去。真实宿主只在最后一步（真下载、真正起进程）才有区别，所以「状态机有没有走对、文案对不对、按钮亮不亮」这类问题都能这样查出来。缺省表单放 `form-<action>.json`，一轮里写回的 kv 由脚本自己累积，相当于宿主的持久化。

```
src/main.rs    WASI 入口、manifest、按 action 分发
src/deploy.rs 一键部署状态机（prep → dl → unzip → find → secret → cfg → run → health）
src/svc.rs    启停服务、面板刷新
src/store.rs  Telegram 登录、建频道、加机器人
src/rc.rs     建 remote、挂载、卸载
src/api.rs    teldrive HTTP 调用与鉴权头
src/cfg.rs    config.yaml 与连接串拼装
src/paths.rs  数据目录布局
src/proto.rs  need/ops 应答与 kv 读写
src/i18n.rs   zh/en 文案
```

## 许可

MIT，见 [LICENSE](LICENSE)。

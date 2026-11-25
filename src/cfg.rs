// 配置文件与两个辅助 PowerShell 脚本的生成。
//
// 为什么不直接写全部配置项：teldrive 的默认值就是官方的 config.sample.yaml，
// 这里只覆盖与部署方式相关的几项（监听端口、数据库地址、密钥），其余交给默认值。
// 这样官方升级新增的键不会因为我们的模板过时而失效。

/// teldrive 的配置。db_url 里可能带 `?sslmode=disable`，不是 YAML 特殊字符，可裸写；
/// 但密钥是 base64，含 `+` `/` `=`，统一加双引号最稳。
pub fn config_yaml(
    port: u16,
    db_url: &str,
    signing_key: &str,
    data_key: &str,
    proxy: &str,
) -> String {
    // 只有真填了代理才写这一行。留空时 config.yaml 里根本没有 telegram.proxy，
    // teldrive 就是直连，也不会撞上「不能和 mtproxy 同时设置」的校验。
    let proxy = proxy.trim().replace('"', "");
    let proxy_line = if proxy.is_empty() {
        String::new()
    } else {
        format!("\x20 proxy: \"{proxy}\"\n")
    };
    format!(
        "# teldrive 配置，由 RCGUI 的 teldrive 插件生成。改端口或数据库后请重新部署。\n\
         http:\n\
        \x20 address: 127.0.0.1:{port}\n\
         database:\n\
        \x20 schema: teldrive\n\
        \x20 url: \"{db_url}\"\n\
        \x20 auto-migrate-legacy: true\n\
         telegram:\n\
        \x20 backend: remote\n\
        {proxy_line}\
        \x20 app-id: 2496\n\
        \x20 app-hash: 8da85b0d5bfe62527e5b244c209159c3\n\
        \x20 upload-threads: 8\n\
        \x20 auto-channel-create: true\n\
        \x20 channel-part-limit: 500000\n\
        \x20 channel-name-prefix: teldrive\n\
         security:\n\
        \x20 signing-key: \"{signing_key}\"\n\
        \x20 data-key: \"{data_key}\"\n\
        \x20 issuer: teldrive-v2\n\
        \x20 allowed-users: []\n\
         logging:\n\
        \x20 log-level: info\n\
        \x20 log-format: json\n",
        port = port,
        db_url = db_url,
        signing_key = signing_key,
        data_key = data_key
    )
}

/// 生成两个密钥。放 PowerShell 里做是因为 WebAssembly 里没有可靠的随机源。
/// 输出两行：hex=<64 字符>，b64=<base64，解码正好 32 字节>。
pub fn secrets_script() -> String {
    ps_file(
        r#"$ErrorActionPreference = 'Stop'
$rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
$h = New-Object byte[] 32
$rng.GetBytes($h)
Write-Output ('hex=' + (($h | ForEach-Object { $_.ToString('x2') }) -join ''))
$b = New-Object byte[] 32
$rng.GetBytes($b)
Write-Output ('b64=' + [Convert]::ToBase64String($b))
"#
        .to_string(),
    )
}

/// 落盘的 .ps1 统一在开头写 UTF-8 BOM。
///
/// Windows PowerShell 5.1 读没有 BOM 的脚本时按系统 ANSI 代码页解码：正文里
/// 只要有一个中文字符，就会被读成别的字节，引号或大括号因此错位，整份脚本
/// 报 "unexpected token"。带 BOM 它才按 UTF-8 读，正文写成 ASCII、路径里带
/// 中文的两种情况都能过。
fn ps_file(body: String) -> String {
    let mut s = String::with_capacity(body.len() + 3);
    s.push('\u{feff}');
    s.push_str(&body);
    s
}

/// 便携版 PostgreSQL：初始化（只做一次）+ 建库，跑完就退出。重复执行不报错。
///
/// 这份脚本**不启动数据库服务**。服务是常驻进程，必须由插件用 `proc.start`
/// 拉起：宿主执行 `proc.run` 时会捕获子进程的 stdout/stderr 并一直读到 EOF，
/// 而 pg_ctl 拉起的 postgres 会继承那两个管道写端，于是 run 永远等不到 EOF、
/// 部署卡在「处理中」直到超时。`proc.start` 不占管道（输出直接落 pg.log），
/// 启动后一秒内宿主就能返回句柄或「立刻退出 + 日志尾」。
///
/// postgres 拒绝以管理员令牌运行，所以这里的失败信息要能让用户看懂怎么办。
pub fn pg_script(pg_bin: &str, pgdata: &str) -> String {
    let initdb = format!("{}\\initdb.exe", pg_bin);
    let pgctl = format!("{}\\pg_ctl.exe", pg_bin);
    let postgres = format!("{}\\postgres.exe", pg_bin);
    ps_file(format!(
        r#"$ErrorActionPreference = 'Stop'
$initdb   = {initdb}
$pgctl    = {pgctl}
$postgres = {postgres}
$data     = {data}

# This file must stay pure ASCII. Windows PowerShell 5.1 reads a .ps1 without
# a byte-order mark as ANSI, so any non-ASCII byte here is decoded as a
# different character and can turn into a stray quote or brace that breaks the
# whole script. Plain ASCII behaves the same under every code page.
# The portable build ships only initdb/pg_ctl/postgres: no psql, no createdb,
# so the database is created through single-user mode (postgres --single reads
# one SQL statement from stdin while the server is stopped). Feeding SQL through
# a pipe hangs in some terminals, so it goes through a file instead.
# This script never starts the server: it exits, and the plugin starts
# postgres.exe with proc.start. A daemon started from a pipe-capturing run
# inherits the pipe and the caller waits for EOF forever.
function Invoke-Single([string]$Tag) {{
  $sql = Join-Path $data "create-$Tag.sql"
  $out = Join-Path $data "create-$Tag.out"
  $err = Join-Path $data "create-$Tag.err"
  $p = Start-Process -FilePath $postgres -ArgumentList @('--single', '-D', $data, 'postgres') `
    -RedirectStandardInput $sql -RedirectStandardOutput $out -RedirectStandardError $err `
    -NoNewWindow -PassThru
  if (-not $p.WaitForExit(60000)) {{ $p.Kill(); return 'ERR single-user mode did not finish within 60s' }}
  $text = ''
  foreach ($f in @($out, $err)) {{
    if (Test-Path $f) {{ $text += (Get-Content -LiteralPath $f -Raw -ErrorAction SilentlyContinue) }}
  }}
  return $text
}}

if (-not (Test-Path (Join-Path $data 'PG_VERSION'))) {{
  New-Item -ItemType Directory -Force -Path $data | Out-Null
  & $initdb -D $data -U postgres --auth=trust --encoding=UTF8 --locale=C | Out-Null
  if ($LASTEXITCODE -ne 0) {{ Write-Output 'ERR initdb failed, see the output above'; exit 1 }}
}}

# A running server means a previous deploy already created the database, and
# single-user mode needs it stopped. Report success and let the caller reuse it.
& $pgctl -D $data status *> $null
if ($LASTEXITCODE -eq 0) {{ Write-Output 'DONE'; exit 0 }}

Set-Content -LiteralPath (Join-Path $data 'create-teldrive.sql') -Value 'CREATE DATABASE teldrive;' -Encoding ASCII
$r = Invoke-Single 'teldrive'
if ($r -match 'ERROR' -and $r -notmatch 'already exists') {{
  Write-Output ('ERR could not create the database: ' + (($r -replace '\s+', ' ').Trim()))
  exit 1
}}
Write-Output 'DONE'
"#,
        initdb = ps_quote(&initdb),
        pgctl = ps_quote(&pgctl),
        postgres = ps_quote(&postgres),
        data = ps_quote(pgdata)
    ))
}

/// PowerShell 单引号字符串：内部的单引号要写成两个。路径里常有空格，必须加引号。
pub fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// 连接串。密码里有 `@` `:` 的话要让用户自己负责，这里只做最基本拼接。
pub fn pg_url(port: u16, user: &str, password: &str, db: &str) -> String {
    format!(
        "postgres://{}:{}@127.0.0.1:{}/{}?sslmode=disable",
        user, password, port, db
    )
}

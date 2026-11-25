// 一键部署的状态机。
//
// 一次调用最多 16 轮 need↔provided，而下载解包、初始化数据库、启动服务、等健康检查
// 加起来步数不少，所以每一步只问一批能并行的能力：
//
//   ""      环境信息 + 数据库端口探测 + docker 是否存在   → 决定 db_mode
//   prep    建目录、写两个 .ps1、并发下载 teldrive/rclone/pg
//   dl      解包这三个下载
//   untar   便携数据库多一层：下载的是 jar，里面的 .txz 还得用 tar 再解一次
//   unzip   找两个 exe，同时跑 secrets.ps1
//   find    把 exe 复制进 bin、跑 pg.ps1（或 docker run）
//   secret  解析密钥，写 config.yaml
//   cfg     检查写盘结果，启动 teldrive
//   run     等 /health/ready
//   health  （最多重试 3 次）
//   final   合并写 kv._state
//
// 「检查再决定」一律放到下一轮，因为同一轮里拿不到自己刚发出的 need 的结果。

use crate::cfg;
use crate::i18n::{f, t};
use crate::paths::Paths;
use crate::proto::*;
use serde_json::{json, Value};

const TELDRIVE_URL: &str =
    "https://github.com/tgdrive/teldrive/releases/download/2.0.14/teldrive-2.0.14-windows-amd64.zip";
const RCLONE_URL: &str =
    "https://github.com/tgdrive/rclone/releases/download/v1.76.0/rclone-v1.76.0-windows-amd64.zip";
// 便携数据库用 zonky 给测试用的 PostgreSQL 构建：22 MB 的 jar 里装着一个 txz 包，
// 解开约 97 MB。PostgreSQL 官方 Windows 安装包（EDB）是 314 MB、解开 1.2 GB，
// 对「不想装数据库」的用户来说代价太大，所以换成这个；代价是下载源在 Maven Central，
// 并且需要系统自带 tar.exe（Windows 10 1803 起有）来解那层 txz。
const PG_URL: &str = "https://repo1.maven.org/maven2/io/zonky/test/postgres/embedded-postgres-binaries-windows-amd64/17.6.0/embedded-postgres-binaries-windows-amd64-17.6.0.jar";
/// `PG_URL` 里那层 txz 的文件名；解 jar 之后就在 `pg` 目录里。
const PG_TXZ: &str = "postgres-windows-x86_64.txz";

/// 部署的 teldrive 版本。`TELDRIVE_URL` 里那串 2.0.14 必须跟着它一起改。
pub const TELDRIVE_VER: &str = "2.0.14";

const MAX_TELDRIVE_ZIP: u64 = 100 * 1024 * 1024;
const MAX_RCLONE_ZIP: u64 = 120 * 1024 * 1024;
const MAX_PG_ZIP: u64 = 40 * 1024 * 1024;

/// 解包完之后的下一步：找两个 exe，顺带把 secrets.ps1 跑掉
/// （它只读系统随机源生成两把密钥，不依赖任何解包结果）。
fn find_list(p: &Paths) -> Vec<Value> {
    vec![
        need_item("fs.find", "f1", json!({
            "dir": format!("{}\\teldrive", p.tmp),
            "name": "teldrive.exe",
            "max_depth": 6
        })),
        need_item("fs.find", "f2", json!({
            "dir": format!("{}\\rclone", p.tmp),
            "name": "rclone.exe",
            "max_depth": 6
        })),
        need_item("proc.run", "secret", json!({
            "program": "powershell.exe",
            "args": [
                "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
                "-File", p.secrets_script()
            ],
            "timeout_ms": 60000
        })),
    ]
}

/// 起 teldrive 本体：端口已经通了之后的一步。
fn run_need(id: u64, st: Value, p: &Paths) -> Value {
    let st = keep(st, "step", json!("run"));
    let list = vec![need_item("proc.start", "start", json!({
        "program": p.teldrive_exe(),
        "args": ["run", "--config", p.cfg],
        "cwd": p.root,
        // silent=true 时宿主用 CREATE_NO_WINDOW 起进程，不弹控制台窗口；
        // 同时必须给 log，否则 teldrive 的输出会被丢掉。
        "silent": true,
        "log": p.log_file,
        // 部署完就把服务交给用户：退出 RCGUI 不该把刚起好的服务带走。
        "keep_on_exit": true
    }))];
    need(id, st, list)
}

/// `net.port_open` 的结果：宿主回 `{open: bool}`，缺这个结果就当没开。
fn is_open(provided: &Value, key: &str) -> bool {
    got(provided, key)
        .get("open")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 部署完成后的面板：进度条收尾、按钮恢复、把最终状态写进 kv。
///
/// `pgproc` 是便携数据库进程的句柄（外部数据库时为 null）：停库要用它，
/// 所以要跟着 teldrive 的句柄一起留在 kv 里。
fn finish(id: u64, kv: &Value, root: &str, port: u16, pgport: u16, mode: &str, db_url: &str, proxy: &str, secret: &(String, String), proc: &Value, pgproc: &Value) -> Value {
    let ops = vec![state_op(kv, |st| {
        st.insert("root".into(), json!(root));
        st.insert("port".into(), json!(port));
        // 端口要留着：起服务时要按它探库、停服务时要按它提示。
        st.insert("pgport".into(), json!(pgport));
        st.insert("mode".into(), json!(mode));
        st.insert("db_url".into(), json!(db_url));
        // 存下来是为了下次打开面板时「代理地址」能填回上次用的值。
        st.insert("proxy".into(), json!(proxy));
        st.insert("signing_key".into(), json!(secret.0));
        st.insert("data_key".into(), json!(secret.1));
        st.insert("proc".into(), proc.clone());
        st.insert("pgproc".into(), pgproc.clone());
        st.insert("version".into(), json!(TELDRIVE_VER));
    })];
    done_ops(
        id,
        ops,
        json!({
            "status": t("deploy.done"),
            "lamp": "ok",
            "progress": 100,
            "enabled": { "deploy": true, "start": false, "stop": true },
            "toast": { "level": "info", "message": t("deploy.done") },
        }),
    )
}

pub fn on_deploy(id: u64, rq: &Rq) -> Value {
    let form = &rq.form;
    let step = state_s(&rq.state, "step");

    match step.as_str() {
        // 第一轮：能一次问齐的都问齐，省一轮就是省一轮。
        "" => {
            let pgport = fnum(form, "pg_port", 5432);
            let list = vec![
                need_item("env.info", "env", json!({})),
                need_item("kv.get_all", "kv", json!({})),
                need_item("net.port_open", "pgopen", json!({ "port": pgport, "timeout_ms": 800 })),
                need_item("proc.run", "docker", json!({
                    "program": "docker.exe",
                    "args": ["--version"],
                    "timeout_ms": 15000
                })),
            ];
            need(id, json!({ "step": "prep" }), list)
        }

        "prep" => {
            if let Some(e) = got_err(&rq.provided, "env") {
                return fail(id, &e);
            }
            let env = got(&rq.provided, "env");
            let data_dir = s(env.get("data_dir"));
            let elevated = env.get("elevated").and_then(|v| v.as_bool()).unwrap_or(false);
            let p = Paths::resolve(&data_dir, &fstr(form, "root"));
            let port = fnum(form, "port", 8080);
            let pgport = fnum(form, "pg_port", 5432);
            let db_url_form = fstr(form, "db_url");
            let pg_open = got(&rq.provided, "pgopen")
                .get("open")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let docker_ok = got(&rq.provided, "docker")
                .get("code")
                .and_then(|v| v.as_i64())
                .map(|c| c == 0)
                .unwrap_or(false);

            let mode = match fstr(form, "db_mode").as_str() {
                "portable" => "portable",
                "docker" => "docker",
                "existing" => "existing",
                // 自动模式也以便携库为首选：它不依赖本机已经装了什么。
                // 只有提权运行时便携库起不来（PostgreSQL 拒绝以管理员身份运行），
                // 这时才去看现成的库或 docker；两样都没有就还是报便携库的限制。
                _ => {
                    if elevated && pg_open {
                        "existing"
                    } else if elevated && docker_ok {
                        "docker"
                    } else {
                        "portable"
                    }
                }
            };

            // 「已有数据库」唯一的前提是连接串。auto 模式因为本机 5432 开着才选到它，
            // 端口本身已经探过了；用户亲口选它时连接串可能指向别的机器，
            // 而 net.port_open 只能探 127.0.0.1，拿本机端口当门槛会挡掉云端 Postgres 用户。
            // 真连不通的话，后面的健康检查会明确报出来。
            if mode == "existing" && db_url_form.is_empty() {
                return fail(id, &t("deploy.need_db_url"));
            }
            if mode == "docker" && !docker_ok {
                return fail(id, &t("deploy.docker_missing"));
            }
            if mode == "portable" && elevated {
                return fail(id, &t("deploy.elevated"));
            }

            let db_url = if mode == "existing" {
                db_url_form.clone()
            } else {
                cfg::pg_url(pgport, "postgres", "postgres", "teldrive")
            };

            let mut list = vec![
                need_item("fs.mkdir", "mk_root", json!({ "path": p.root })),
                need_item("fs.mkdir", "mk_bin", json!({ "path": p.bin })),
                need_item("fs.mkdir", "mk_tmp", json!({ "path": p.tmp })),
                need_item("fs.mkdir", "mk_scripts", json!({ "path": p.scripts })),
                need_item("fs.mkdir", "mk_logs", json!({ "path": p.logs })),
                need_item("fs.write_text", "w_secrets", json!({
                    "path": p.secrets_script(),
                    "text": cfg::secrets_script()
                })),
                need_item("fs.write_text", "w_pg", json!({
                    "path": p.pg_script(),
                    "text": cfg::pg_script(&p.pg_bin(), &p.pgdata)
                })),
                need_item("http.download", "dl1", json!({
                    "url": TELDRIVE_URL,
                    "dest": format!("{}\\teldrive.zip", p.tmp),
                    "max_bytes": MAX_TELDRIVE_ZIP
                })),
                need_item("http.download", "dl2", json!({
                    "url": RCLONE_URL,
                    "dest": format!("{}\\rclone.zip", p.tmp),
                    "max_bytes": MAX_RCLONE_ZIP
                })),
            ];
            if mode == "portable" {
                // tar 要往这个目录里解，先建出来；jar 本身解到 p.pg。
                list.push(need_item("fs.mkdir", "mk_pgsql", json!({
                    "path": format!("{}\\pgsql", p.pg)
                })));
                list.push(need_item("http.download", "dl3", json!({
                    "url": PG_URL,
                    "dest": format!("{}\\pg.jar", p.tmp),
                    "max_bytes": MAX_PG_ZIP
                })));
            }

            let st = json!({
                "step": "dl",
                "root": p.root,
                "port": port,
                "pgport": pgport,
                "mode": mode,
                "db_url": db_url,
                // 代理只在第一轮的表单里拿得到，写 config.yaml 时要一路带下去。
                "proxy": fstr(form, "proxy"),
                // kv 只在第一轮取得到；后面几轮 provided 里只有本轮问的东西，
                // 所以要写 kv 时得靠这份快照拼出合并写入。
                "kv": got(&rq.provided, "kv"),
            });
            need(id, st, list)
        }

        "dl" => {
            for (key, name) in [("dl1", "teldrive"), ("dl2", "rclone"), ("dl3", "pg")] {
                if let Some(e) = got_err(&rq.provided, key) {
                    return fail(id, &format!("{}（{}）：{}", t("deploy.step.dl"), name, e));
                }
            }
            let p = Paths::new(&state_s(&rq.state, "root"));
            let mode = state_s(&rq.state, "mode");
            let mut list = vec![
                need_item("fs.unzip", "u1", json!({
                    "zip": format!("{}\\teldrive.zip", p.tmp),
                    "dest": format!("{}\\teldrive", p.tmp)
                })),
                need_item("fs.unzip", "u2", json!({
                    "zip": format!("{}\\rclone.zip", p.tmp),
                    "dest": format!("{}\\rclone", p.tmp)
                })),
            ];
            if mode == "portable" {
                list.push(need_item("fs.unzip", "u3", json!({
                    "zip": format!("{}\\pg.jar", p.tmp),
                    "dest": p.pg
                })));
            }
            let st = keep(rq.state.clone(), "step", json!("unzip"));
            need(id, st, list)
        }

        "unzip" => {
            for (key, name) in [("u1", "teldrive"), ("u2", "rclone"), ("u3", "pg")] {
                if let Some(e) = got_err(&rq.provided, key) {
                    return fail(id, &format!("{}（{}）：{}", t("deploy.step.unzip"), name, e));
                }
            }
            let p = Paths::new(&state_s(&rq.state, "root"));
            let mode = state_s(&rq.state, "mode");
            if mode == "portable" {
                // 便携数据库下来的是 jar，里面的 txz 还得用系统自带的 tar 再解一层。
                // tar 必须等上一步 fs.unzip 落盘，所以只能排到这一轮。
                let list = vec![need_item("proc.run", "pgx", json!({
                    "program": "tar.exe",
                    "args": [
                        "-xf", format!("{}\\{}", p.pg, PG_TXZ),
                        "-C", format!("{}\\pgsql", p.pg)
                    ],
                    "timeout_ms": 300000
                }))];
                let st = keep(rq.state.clone(), "step", json!("untar"));
                return need(id, st, list);
            }
            let st = keep(rq.state.clone(), "step", json!("find"));
            need(id, st, find_list(&p))
        }

        // 便携数据库多出来的这一轮：检查 tar 有没有把 txz 解开，然后回到正常流程。
        "untar" => {
            let res = got(&rq.provided, "pgx");
            let code = res.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
            let stderr = s(res.get("stderr"));
            if let Some(e) = got_err(&rq.provided, "pgx") {
                return fail(id, &f(&t("deploy.untar_fail"), &[&e]));
            }
            if code != 0 {
                let msg = if stderr.is_empty() { format!("tar.exe 退出码 {}", code) } else { truncate(&stderr, 300) };
                return fail(id, &f(&t("deploy.untar_fail"), &[&msg]));
            }
            let p = Paths::new(&state_s(&rq.state, "root"));
            let st = keep(rq.state.clone(), "step", json!("find"));
            need(id, st, find_list(&p))
        }

        "find" => {
            if let Some(e) = got_err(&rq.provided, "secret") {
                return fail(id, &format!("{}：{}", t("deploy.step.secret"), e));
            }
            let teldrive = s(got(&rq.provided, "f1").get("path"));
            let rclone = s(got(&rq.provided, "f2").get("path"));
            if teldrive.is_empty() {
                return fail(id, &format!("{}：teldrive.exe", t("deploy.step.find")));
            }
            if rclone.is_empty() {
                return fail(id, &format!("{}：rclone.exe", t("deploy.step.find")));
            }

            let p = Paths::new(&state_s(&rq.state, "root"));
            let mode = state_s(&rq.state, "mode");
            let pgport = state_s(&rq.state, "pgport").parse::<u16>().unwrap_or(5432);

            let mut list = vec![
                need_item("fs.copy", "c1", json!({ "from": teldrive, "to": p.teldrive_exe() })),
                need_item("fs.copy", "c2", json!({ "from": rclone, "to": p.rclone_exe() })),
            ];
            if mode == "portable" {
                list.push(need_item("proc.run", "pg", json!({
                    "program": "powershell.exe",
                    "args": [
                        "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
                        "-File", p.pg_script()
                    ],
                    "timeout_ms": 600000
                })));
            } else if mode == "docker" {
                list.push(need_item("proc.run", "pg", json!({
                    "program": "docker.exe",
                    "args": [
                        "run", "-d", "--name", "teldrive-pg",
                        "-e", "POSTGRES_PASSWORD=postgres",
                        "-e", "POSTGRES_DB=teldrive",
                        "-p", format!("127.0.0.1:{}:5432", pgport),
                        "postgres:17-alpine"
                    ],
                    "timeout_ms": 180000
                })));
            }

            let st = keep(rq.state.clone(), "step", json!("secret"));
            let st = keep(
                st,
                "secret_raw",
                json!(s(got(&rq.provided, "secret").get("stdout"))),
            );
            need(id, st, list)
        }

        "secret" => {
            for key in ["c1", "c2"] {
                if let Some(e) = got_err(&rq.provided, key) {
                    return fail(id, &format!("{}：{}", t("deploy.step.place"), e));
                }
            }
            let mode = state_s(&rq.state, "mode");
            if mode == "portable" || mode == "docker" {
                let pg = got(&rq.provided, "pg");
                if let Some(e) = got_err(&rq.provided, "pg") {
                    return fail(id, &format!("{}：{}", t("deploy.step.db"), e));
                }
                let code = pg.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
                let out = s(pg.get("stdout")) + &s(pg.get("stderr"));
                let benign = out.contains("already in use") || out.contains("already exists");
                if code != 0 && !benign {
                    return fail(id, &format!("{}：{}", t("deploy.step.db"), truncate(&out, 300)));
                }
            }

            let raw = state_s(&rq.state, "secret_raw");
            let mut hex = String::new();
            let mut b64 = String::new();
            for line in raw.lines() {
                let line = line.trim();
                if let Some(v) = line.strip_prefix("hex=") {
                    hex = v.trim().to_string();
                } else if let Some(v) = line.strip_prefix("b64=") {
                    b64 = v.trim().to_string();
                }
            }
            if hex.len() < 32 || b64.is_empty() {
                return fail(id, &format!("{}：{}", t("deploy.step.secret"), truncate(&raw, 200)));
            }

            let p = Paths::new(&state_s(&rq.state, "root"));
            let port = state_s(&rq.state, "port").parse::<u16>().unwrap_or(8080);
            let db_url = state_s(&rq.state, "db_url");
            let proxy = state_s(&rq.state, "proxy");
            let text = cfg::config_yaml(port, &db_url, &hex, &b64, &proxy);

            // 便携数据库起服务放在单独一轮：这一轮只有写文件，拿不到进程句柄，
            // 也判断不了端口有没有开。
            let next = if mode == "portable" { "pgstart" } else { "cfg" };
            let st = keep(rq.state.clone(), "step", json!(next));
            let st = keep(st, "hex", json!(hex));
            let st = keep(st, "b64", json!(b64));
            let list = vec![need_item("fs.write_text", "w_cfg", json!({
                "path": p.cfg,
                "text": text
            }))];
            need(id, st, list)
        }

        // 便携数据库：用 proc.start 拉起 postgres.exe。
        //
        // 不能用 proc.run —— 宿主会把子进程的 stdout/stderr 接管并读到 EOF，
        // 而常驻的 postgres 一直握着这两个管道，run 就永远不返回。
        // proc.start 的输出直接追加进 logs\pg.log，启动后一秒内宿主就会返回句柄，
        // 或者返回「进程启动后立刻退出 + 日志末尾」。
        "pgstart" => {
            let p = Paths::new(&state_s(&rq.state, "root"));
            let pgport = state_s(&rq.state, "pgport").parse::<u16>().unwrap_or(5432);
            let st = keep(rq.state.clone(), "step", json!("pgwait"));
            let list = vec![
                need_item("proc.start", "pg", json!({
                    "program": p.pg_exe(),
                    "args": [
                        "-D", p.pgdata,
                        "-p", pgport.to_string(),
                        "-c", "listen_addresses=127.0.0.1"
                    ],
                    "cwd": p.pg_bin(),
                    "silent": true,
                    "log": p.pg_log(),
                    // 部署结束时用户可能马上关掉 RCGUI 去挂载，库必须留着。
                    "keep_on_exit": true
                })),
                need_item("net.port_open", "open", json!({
                    "port": pgport,
                    "timeout_ms": 3000
                })),
            ];
            need(id, st, list)
        }

        // 端口连得上才算数据库可用。proc.start 那一秒的观察只能证明进程没立刻死，
        // 不能证明它已经在监听。
        "pgwait" => {
            let p = Paths::new(&state_s(&rq.state, "root"));
            let pgport = state_s(&rq.state, "pgport").parse::<u16>().unwrap_or(5432);
            let open = is_open(&rq.provided, "open");
            if let Some(e) = got_err(&rq.provided, "pg") {
                // 端口已经有人在听：多半是上次部署留下的服务还活着，直接用它。
                if open {
                    let st = keep(rq.state.clone(), "pgproc", Value::Null);
                    return run_need(id, st, &p);
                }
                return fail(id, &f(&t("deploy.pg_start_fail"), &[&e]));
            }
            let started = got(&rq.provided, "pg");
            let st = keep(rq.state.clone(), "pgproc", json!({
                "handle": s(started.get("handle")),
                "pid": started.get("pid").cloned().unwrap_or(Value::Null)
            }));
            if open {
                return run_need(id, st, &p);
            }
            let tries = rq.state.get("pgtries").and_then(|v| v.as_u64()).unwrap_or(0);
            if tries >= 2 {
                return fail(id, &f(&t("deploy.pg_not_ready"), &[&pgport.to_string(), &p.pg_log()]));
            }
            let st = keep(st, "pgtries", json!(tries + 1));
            let st = keep(st, "step", json!("pgrecheck"));
            let list = vec![need_item("net.port_open", "open", json!({
                "port": pgport,
                "timeout_ms": 5000
            }))];
            need(id, st, list)
        }

        // 再多给一次机会：postgres 初始化完到能连上有时要一两秒。
        "pgrecheck" => {
            let p = Paths::new(&state_s(&rq.state, "root"));
            let pgport = state_s(&rq.state, "pgport").parse::<u16>().unwrap_or(5432);
            if is_open(&rq.provided, "open") {
                return run_need(id, rq.state.clone(), &p);
            }
            fail(id, &f(&t("deploy.pg_not_ready"), &[&pgport.to_string(), &p.pg_log()]))
        }

        "cfg" => {
            if let Some(e) = got_err(&rq.provided, "w_cfg") {
                return fail(id, &format!("{}：{}", t("deploy.step.cfg"), e));
            }
            let p = Paths::new(&state_s(&rq.state, "root"));
            run_need(id, rq.state.clone(), &p)
        }

        "run" => {
            if let Some(e) = got_err(&rq.provided, "start") {
                return fail(id, &format!("{}：{}", t("deploy.step.run"), e));
            }
            let started = got(&rq.provided, "start");
            let port = state_s(&rq.state, "port").parse::<u16>().unwrap_or(8080);
            let st = keep(rq.state.clone(), "step", json!("health"));
            let st = keep(st, "tries", json!(0));
            let st = keep(
                st,
                "proc",
                json!({
                    "handle": s(started.get("handle")),
                    "pid": started.get("pid").cloned().unwrap_or(Value::Null),
                }),
            );
            let list = vec![need_item("http.text", "health", json!({
                "url": format!("http://127.0.0.1:{}/health/ready", port),
                "timeout_ms": 8000
            }))];
            need(id, st, list)
        }

        "health" => {
            let port = state_s(&rq.state, "port").parse::<u16>().unwrap_or(8080);
            let pgport = state_s(&rq.state, "pgport").parse::<u16>().unwrap_or(5432);
            let tries = rq
                .state
                .get("tries")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let health = got(&rq.provided, "health");
            let status = health.get("status").and_then(|v| v.as_u64()).unwrap_or(0);
            let text = s(health.get("body"));

            if status == 200 {
                let kv = rq.state.get("kv").cloned().unwrap_or(Value::Null);
                let mode = state_s(&rq.state, "mode");
                let db_url = state_s(&rq.state, "db_url");
                let hex = state_s(&rq.state, "hex");
                let b64 = state_s(&rq.state, "b64");
                let proc = rq.state.get("proc").cloned().unwrap_or(Value::Null);
                let pgproc = rq.state.get("pgproc").cloned().unwrap_or(Value::Null);
                if text.contains("degraded") {
                    let ops = vec![state_op(&kv, |st| {
                        st.insert("root".into(), json!(state_s(&rq.state, "root")));
                        st.insert("port".into(), json!(port));
                        st.insert("pgport".into(), json!(pgport));
                        st.insert("mode".into(), json!(mode));
                        st.insert("db_url".into(), json!(db_url));
                        st.insert("signing_key".into(), json!(hex));
                        st.insert("data_key".into(), json!(b64));
                        st.insert("proc".into(), proc.clone());
                        st.insert("pgproc".into(), pgproc.clone());
                    })];
                    let msg = f(&t("deploy.health_degraded"), &[&truncate(&text, 120)]);
                    return done_ops(
                        id,
                        ops,
                        json!({
                            "status": msg,
                            "lamp": "warn",
                            "progress": 100,
                            "enabled": { "deploy": true, "start": false, "stop": true },
                            "toast": { "level": "warn", "message": msg },
                        }),
                    );
                }
                return finish(
                    id,
                    &kv,
                    &state_s(&rq.state, "root"),
                    port,
                    pgport,
                    &mode,
                    &db_url,
                    &state_s(&rq.state, "proxy"),
                    &(hex, b64),
                    &proc,
                    &pgproc,
                );
            }

            if tries >= 3 {
                let p = Paths::new(&state_s(&rq.state, "root"));
                let msg = f(
                    &t("deploy.health_fail"),
                    &[&status.to_string(), &p.log_file],
                );
                return fail(id, &msg);
            }

            // 没有 sleep 能力，用 ping 的两次间隔凑两秒，超时由 proc.run 兜底。
            let st = keep(rq.state.clone(), "tries", json!(tries + 1));
            let list = vec![
                need_item("proc.run", "sleep", json!({
                    "program": "cmd.exe",
                    "args": ["/c", "ping -n 3 127.0.0.1 >nul"],
                    "timeout_ms": 8000
                })),
                need_item("http.text", "health", json!({
                    "url": format!("http://127.0.0.1:{}/health/ready", port),
                    "timeout_ms": 8000
                })),
            ];
            need(id, st, list)
        }

        other => fail(id, &f(&t("action.unknown"), &[other])),
    }
}

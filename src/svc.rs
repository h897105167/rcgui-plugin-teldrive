// teldrive 服务的启停与状态。
//
// 三件事：看 `bin\teldrive.exe` 在不在、用它和已有的 `config.yaml` 起一个
// 进程（日志追加到 `logs\teldrive.log`）、把它停掉。
// 「在跑」以端口为准：进程是 `keep_on_exit` 起的，关掉 RCGUI 它还活着，而宿主
// 一重启进程表就空了，只有端口能说明真实状态；句柄和 pid 只用来停止。

use crate::deploy::TELDRIVE_VER;
use crate::i18n::{f, t};
use crate::paths::Paths;
use crate::proto::*;
use crate::store::with_qr;
use serde_json::{json, Value};

const DEFAULT_PORT: u16 = 8080;

/// kv 里记的部署根目录与端口。没部署过时 root 是空串，`Paths` 也就不用当真。
fn ctx(kv: &Value) -> (Paths, u16) {
    let st = kv_state(kv);
    let root = s(st.get("root"));
    let port = st
        .get("port")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_PORT as u64) as u16;
    (Paths::new(&root), port)
}

fn handle_of(kv: &Value) -> String {
    s(kv_state(kv).get("proc").and_then(|p| p.get("handle")))
}

/// 便携数据库进程的句柄。用外部数据库时这里是空的。
fn pg_handle_of(kv: &Value) -> String {
    s(kv_state(kv).get("pgproc").and_then(|p| p.get("handle")))
}

/// 收掉“已在跑的那一个”teldrive 进程，供改完配置后重启用。
///
/// 先按 kv 里的 pid 杀：宿主重启后它的进程句柄表是空的，`proc.stop` 拿着上次
/// 会话的句柄只会报错，而进程因为 `keep_on_exit` 还活着。pid 没有才退回句柄。
/// 两条路都不通时返回 None，调用方据此提示用户手动处理。
pub(crate) fn stop_need(kv: &Value, p: &Paths) -> Option<Value> {
    let pid = pid_of(kv, "proc");
    if pid > 0 {
        return Some(kill_need("stopped", pid, &p.log_file));
    }
    let handle = handle_of(kv);
    if handle.is_empty() {
        return None;
    }
    Some(need_item("proc.stop", "stopped", json!({ "handle": handle })))
}

/// `net.port_open` 的结果。没拿到就当端口没开。
fn open_of(provided: &Value, key: &str) -> bool {
    got(provided, key)
        .get("open")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// kv 里记下的 pid。宿主重启后句柄全失效，只剩它能用来结束残留进程。
fn pid_of(kv: &Value, key: &str) -> u64 {
    kv_state(kv)
        .get(key)
        .and_then(|p| p.get("pid"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
}

/// 便携数据库的端口。部署时记进 kv，没记就按 PostgreSQL 的默认值。
fn pg_port_of(kv: &Value) -> u16 {
    kv_state(kv)
        .get("pgport")
        .and_then(|v| v.as_u64())
        .unwrap_or(5432) as u16
}

fn uses_portable_db(kv: &Value) -> bool {
    s(kv_state(kv).get("mode")) == "portable"
}

/// 一次 `action:"state"`（含 on_start 的静默探测）的完整面板。
///
/// `pg_open` 是便携数据库端口的状态；用外部数据库时传 false，那一行检查不显示。
fn panel(kv: &Value, port: u16, exe: bool, cfg: bool, running: bool, pg_open: bool) -> Value {
    // 只为在 checks 里显示文件路径；ctx 只读 kv，重复调用没有副作用。
    let (p, _) = ctx(kv);
    let exe_file = p.teldrive_exe();
    let cfg_file = p.cfg;
    let status = if !exe {
        t("svc.not_deployed")
    } else if !cfg {
        f(&t("env.status.partial"), &["config.yaml"])
    } else if running {
        f(&t("svc.running"), &[&port.to_string()])
    } else {
        t("svc.stopped")
    };
    let lamp = if !exe || !cfg {
        "warn"
    } else if running {
        "ok"
    } else {
        "warn"
    };

    let mut values = serde_json::Map::new();
    if exe {
        values.insert("port".into(), json!(port));
        let ver = {
            let v = s(kv_state(kv).get("version"));
            if v.is_empty() {
                TELDRIVE_VER.to_string()
            } else {
                v
            }
        };
        values.insert("deployed_version".into(), json!(ver));
    }

    let mut checks = vec![
        json!({ "label": t("deploy.file.teldrive"), "value": exe_file, "ok": exe }),
        json!({ "label": "config.yaml", "value": cfg_file, "ok": cfg }),
        json!({ "label": f(&t("svc.running"), &[&port.to_string()]), "value": "", "ok": running }),
    ];    if uses_portable_db(kv) {
        // 便携库是本插件的责任：和 teldrive 一样列出来，端口通不通一眼能看到。
        let pg_port = pg_port_of(kv);
        checks.push(json!({
            "label": f(&t("svc.pg_check"), &[&pg_port.to_string()]),
            "value": "",
            "ok": pg_open
        }));
    }

    // 登录相关的按钮在 store::panel 里也判一次，两处共用同一套规则。
    let logged = !s(kv_state(kv).get("api_key")).is_empty();
    let (flow, need_pw) = crate::store::flow_of(kv);

    let mut body = json!({
        "status": status,
        "lamp": lamp,
        "values": Value::Object(values),
        "checks": checks,
        "hints": { "hint_svc": t("hint.teldrive") },
        "enabled": {
            "deploy": true,
            "start": exe && cfg && !running,
            "stop": running,
            "open_dir": exe,
            // 登录相关按钮这里也要判：刷新状态是最常点的动作，宿主把「没提到的控件」
            // 当作可用，只写 deployment 那几个键的话，登录后刷新一下它们又亮了。
            "login_start": !logged,
            "qr_start": !logged,
            "qr_poll": !logged && !flow.is_empty() && !need_pw,
            "login_verify": !logged && !flow.is_empty() && !need_pw,
            "login_password": !logged && need_pw,
            "logout": logged,
        }
    });
    // 频道下拉、机器人清单、登录后隐藏 ④ 段这些两个面板都要一致：
    // 刷新状态是用户最常点的动作，只在「④ Telegram 登录」面板里隐藏的话，
    // 点一下刷新整段又冒出来了。
    crate::store::decorate(kv, &mut body);
    body
}

pub fn on_state(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        // 端口存在 kv 里，所以得先取 kv 才知道去探哪个端口。
        "" => need(
            id,
            json!({ "step": "meta" }),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),

        "meta" => {
            let kv = got(&rq.provided, "kv");
            if let Some(e) = got_err(&rq.provided, "kv") {
                return fail(id, &e);
            }
            let (p, port) = ctx(&kv);
            if p.root.is_empty() {
                return done(id, panel(&kv, port, false, false, false, false));
            }
            let mut list = vec![
                need_item("fs.meta", "exe", json!({ "path": p.teldrive_exe() })),
                need_item("fs.meta", "cfgmet", json!({ "path": p.cfg })),
                need_item(
                    "net.port_open",
                    "open",
                    json!({ "port": port, "timeout_ms": 800 }),
                ),
            ];
            let h = handle_of(&kv);
            if !h.is_empty() {
                list.push(need_item("proc.status", "proc", json!({ "handle": h })));
            }
            if uses_portable_db(&kv) {
                // 顺手看一眼数据库端口，面板上那条检查要用。
                list.push(need_item(
                    "net.port_open",
                    "pgopen",
                    json!({ "port": pg_port_of(&kv), "timeout_ms": 500 }),
                ));
            }
            need(
                id,
                json!({ "step": "report", "kv": kv, "port": port }),
                list,
            )
        }

        "report" => {
            let kv = rq.state.get("kv").cloned().unwrap_or(Value::Null);
            let port = rq
                .state
                .get("port")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_PORT as u64) as u16;
            let exe = got(&rq.provided, "exe")
                .get("exists")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let cfg = got(&rq.provided, "cfgmet")
                .get("exists")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let open = got(&rq.provided, "open")
                .get("open")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let proc_running = got(&rq.provided, "proc")
                .get("running")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let pg_open = got(&rq.provided, "pgopen")
                .get("open")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            // 端口通但没有句柄 = 进程是别人起的（例如手动跑的命令行）。
            let running = proc_running || (open && exe);
            let mut body = panel(&kv, port, exe, cfg, running, pg_open);
            // 关掉面板再打开时，扫码那张图还在磁盘上、路径还记在 kv 里，
            // 所以照样挂上去 —— 否则用户刚扫一半，切个页面回来码就没了。
            if !s(kv_state(&kv).get("login").and_then(|l| l.get("qr_url"))).is_empty() {
                body = with_qr(body, &ctx(&kv).0);
            }
            done(id, body)
        }

        _ => fail(id, &t("action.unknown")),
    }
}

pub fn on_start(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            json!({ "step": "go" }),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),

        "go" => {
            let kv = got(&rq.provided, "kv");
            let (p, port) = ctx(&kv);
            if p.root.is_empty() {
                return fail(id, &t("deploy.need_path"));
            }
            need(
                id,
                json!({ "step": "meta", "kv": kv }),
                vec![
                    need_item("fs.meta", "exe", json!({ "path": p.teldrive_exe() })),
                    // 探端口就知道服务还在不在：RCGUI 重启后宿主进程表是空的，
                    // kv 里留下的句柄查不出真实状态，而进程本身可能还活着。
                    need_item("net.port_open", "topen", json!({ "port": port, "timeout_ms": 500 })),
                ],
            )
        }

        "meta" => {
            let kv = rq.state.get("kv").cloned().unwrap_or(Value::Null);
            let (p, port) = ctx(&kv);
            if !got(&rq.provided, "exe")
                .get("exists")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                return fail(id, &t("svc.not_deployed"));
            }
            // 端口通着就是已经在跑了 —— 可能是上一次 RCGUI 会话留下的（keep_on_exit
            // 让进程活到了现在），那种情况下 kv 里的句柄早就失效，只有端口说实话。
            if open_of(&rq.provided, "topen") {
                return done(
                    id,
                    json!({
                        "status": f(&t("svc.running"), &[&port.to_string()]),
                        "lamp": "ok",
                        "enabled": { "start": false, "stop": true },
                    }),
                );
            }
            // 便携库平时也由我们起：teldrive 连不上它就没法启动。
            // 这里每次都探端口，不信 kv 里的 pgproc 句柄：上一个 RCGUI 会话留下的
            // 句柄早就不在进程表里了，信它就会直接起 teldrive 然后连库失败。
            if uses_portable_db(&kv) {
                return need(
                    id,
                    json!({ "step": "pgprobe", "kv": kv, "port": port }),
                    vec![need_item(
                        "net.port_open",
                        "pgopen",
                        json!({ "port": pg_port_of(&kv), "timeout_ms": 800 }),
                    )],
                );
            }
            start_need(id, &kv, &p, port, &Value::Null)
        }

        // 库端口不通才去拉 postgres；通了说明已经有实例，别起第二个。
        "pgprobe" => {
            let kv = rq.state.get("kv").cloned().unwrap_or(Value::Null);
            let (p, port) = ctx(&kv);
            if got(&rq.provided, "pgopen")
                .get("open")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                // 端口已经通了：库里可能还留着我们自己起的那个实例的句柄，带上它，
                // 免得 report 又把句柄写成空、之后「停止」停不掉库。
                let pgproc = kv_state(&kv).get("pgproc").cloned().unwrap_or(Value::Null);
                return start_need(id, &kv, &p, port, &pgproc);
            }
            let pg_port = pg_port_of(&kv).to_string();
            need(
                id,
                json!({ "step": "pgup", "kv": kv, "port": port }),
                vec![need_item(
                    "proc.start",
                    "pgproc",
                    json!({
                        "program": p.pg_exe(),
                        "args": ["-D", p.pgdata, "-p", pg_port, "-c", "listen_addresses=127.0.0.1"],
                        "cwd": p.pg_bin(),
                        "silent": true,
                        "log": p.pg_log(),
                        // 库要活到用户主动停它为止：关掉 RCGUI 就停库，正在考的
                        // 挂载立刻失效，下次开 GUI 还得重来一遍。
                        "keep_on_exit": true,
                    }),
                )],
            )
        }

        // postgres 刚起，宿主已经确认它没立刻退出；把句柄带上继续起 teldrive。
        "pgup" => {
            let kv = rq.state.get("kv").cloned().unwrap_or(Value::Null);
            let (p, port) = ctx(&kv);
            if let Some(e) = got_err(&rq.provided, "pgproc") {
                return fail(id, &f(&t("svc.pg_start_fail"), &[&e]));
            }
            let pgproc = got(&rq.provided, "pgproc");
            start_need(id, &kv, &p, port, &pgproc)
        }

        // 进程刚起，给它一秒，然后同时问「还活着吗」和「健康检查通吗」。
        "up" => {
            let kv = rq.state.get("kv").cloned().unwrap_or(Value::Null);
            let port = rq
                .state
                .get("port")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_PORT as u64) as u16;
            if let Some(e) = got_err(&rq.provided, "proc") {
                return fail(id, &f(&t("svc.start_fail"), &[&e]));
            }
            let proc = got(&rq.provided, "proc");
            if proc.get("handle").and_then(|v| v.as_str()).unwrap_or("").is_empty() {
                // proc.start 没给句柄：说明它已经退出了，后面的 status 查不到。
                let (p, _) = ctx(&kv);
                return fail(id, &f(&t("svc.instant_exit"), &[&p.log_file]));
            }
            let mut st = json!({ "step": "report", "kv": kv, "port": port, "proc": proc });
            if let Some(pg) = rq.state.get("pgproc") {
                st["pgproc"] = pg.clone();
            }
            need(
                id,
                st,
                vec![
                    need_item(
                        "proc.status",
                        "alive",
                        json!({ "handle": proc.get("handle") }),
                    ),
                    need_item(
                        "http.text",
                        "health",
                        json!({
                            "url": format!("http://127.0.0.1:{}/health/live", port),
                            "timeout_ms": 8000
                        }),
                    ),
                ],
            )
        }

        "report" => {
            let kv = rq.state.get("kv").cloned().unwrap_or(Value::Null);
            // 只为拼日志路径；ctx 只读 kv，不会碰进程。
            let (p, _) = ctx(&kv);
            let port = rq
                .state
                .get("port")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_PORT as u64) as u16;
            let proc = rq.state.get("proc").cloned().unwrap_or(Value::Null);
            let alive = got(&rq.provided, "alive")
                .get("running")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let code = got(&rq.provided, "health")
                .get("status")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let pgproc = rq.state.get("pgproc").cloned().unwrap_or(Value::Null);
            let ops = vec![state_op(&kv, |st| {
                st.insert("proc".into(), proc.clone());
                if !pgproc.is_null() {
                    st.insert("pgproc".into(), pgproc.clone());
                }
            })];
            let base = json!({
                "enabled": { "start": false, "stop": true },
            });
            let mut body = base;
            let failed = !alive;
            if failed {
                // 进程立刻退出了：把日志路径交出去，让用户自己看最后几行。
                body["status"] = json!(f(&t("svc.instant_exit"), &[&p.log_file]));
                body["lamp"] = json!("err");
                body["enabled"]["stop"] = json!(false);
                body["hints"] = json!({ "hint_svc": f(&t("svc.log_hint"), &[&p.log_file]) });
            } else if code == 200 {
                body["status"] = json!(f(&t("svc.running"), &[&port.to_string()]));
                body["lamp"] = json!("ok");
                body["toast"] = json!({ "level": "info", "message": t("svc.started") });
            } else {
                // 进程活着但健康检查还没好（迁移建表要几秒）：报 warn，不拦用户。
                body["status"] = json!(f(&t("svc.health_wait"), &[&code.to_string()]));
                body["lamp"] = json!("warn");
            }
            done_ops(id, ops, body)
        }

        _ => fail(id, &t("action.unknown")),
    }
}

/// 句柄随宿主重启丢了之后，按 pid 结束整棵进程树。
///
/// `taskkill` 自己会退出，所以能用捕获型 `proc.run`；但 pid 早就不存在时它会
/// 以非 0 退出，这里的调用方（停止流程）本来就不看这个错。
fn kill_need(key: &str, pid: u64, log: &str) -> Value {
    need_item(
        "proc.start",
        key,
        json!({
            "program": "taskkill.exe",
            "args": ["/PID", pid.to_string(), "/T", "/F"],
            "silent": true,
            "log": log,
        }),
    )
}

fn start_need(id: u64, kv: &Value, p: &Paths, port: u16, pgproc: &Value) -> Value {
    let list = vec![need_item(
        "proc.start",
        "proc",
        json!({
            "program": p.teldrive_exe(),
            "args": ["run", "--config", p.cfg],
            "cwd": p.root,
            // silent=true 时宿主用 CREATE_NO_WINDOW 起进程，不弹控制台窗口；
            // 同时必须给 log，否则 teldrive 的输出会被丢掉。
            "silent": true,
            "log": p.log_file,
            // 服务要活到用户主动停它为止：rclone 的 teldrive 后端全靠这个
            // 进程回话，关掉 GUI 就把它带走，正在用的挂载会立刻失效。
            "keep_on_exit": true,
        }),
    )];
    let mut st = json!({ "step": "up", "kv": kv, "port": port });
    // 便携库的句柄要一路带到最后的 report 才写进 kv；空句柄=没起库。
    if !s(pgproc.get("handle")).is_empty() {
        st["pgproc"] = pgproc.clone();
    }
    need(id, st, list)
}

pub fn on_stop(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            json!({ "step": "go" }),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),

        "go" => {
            let kv = got(&rq.provided, "kv");
            let (p, port) = ctx(&kv);
            let h = handle_of(&kv);
            let pgh = pg_handle_of(&kv);
            // 句柄会随宿主重启丢失，而进程因 keep_on_exit 还在跑，这时只剩 kv 里的
            // pid 能把它们收掉；句柄和 pid 都没有才真的没得停。
            let tpid = pid_of(&kv, "proc");
            let gpid = pid_of(&kv, "pgproc");
            if h.is_empty() && pgh.is_empty() && tpid == 0 && gpid == 0 {
                return fail(id, &f(&t("svc.no_handle"), &[&port.to_string()]));
            }
            // 先停 teldrive，再停便携库 —— 反过来的话 teldrive 会一边报连接错误一边退。
            let mut list = Vec::new();
            if !h.is_empty() {
                list.push(need_item("proc.stop", "stop", json!({ "handle": h })));
            } else if tpid > 0 {
                list.push(kill_need("stop", tpid, &p.log_file));
            }
            if !pgh.is_empty() {
                list.push(need_item("proc.stop", "pgstop", json!({ "handle": pgh })));
            } else if gpid > 0 {
                list.push(kill_need("pgstop", gpid, &p.log_file));
            }
            need(id, json!({ "step": "report", "kv": kv }), list)
        }

        "report" => {
            let kv = rq.state.get("kv").cloned().unwrap_or(Value::Null);
            // 进程已经不在时 proc.stop 也会报错，这种错没意义，照样清掉句柄。
            let ops = vec![state_op(&kv, |st| {
                st.insert("proc".into(), Value::Null);
                st.insert("pgproc".into(), Value::Null);
            })];
            done_ops(
                id,
                ops,
                json!({
                    "status": t("svc.stopped_toast"),
                    "lamp": "ok",
                    "enabled": { "start": true, "stop": false },
                    "toast": { "level": "info", "message": t("svc.stopped_toast") },
                }),
            )
        }

        _ => fail(id, &t("action.unknown")),
    }
}

pub fn on_open_dir(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            json!({ "step": "go" }),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),

        "go" => {
            let kv = got(&rq.provided, "kv");
            let (p, _) = ctx(&kv);
            if p.root.is_empty() {
                return fail(id, &t("deploy.need_path"));
            }
            need(
                id,
                json!({ "step": "report" }),
                vec![need_item("fs.open_dir", "opened", json!({ "path": p.root }))],
            )
        }

        "report" => {
            if let Some(e) = got_err(&rq.provided, "opened") {
                return fail(id, &e);
            }
            done(id, json!({ "status": t("ok"), "lamp": "ok" }))
        }

        _ => fail(id, &t("action.unknown")),
    }
}

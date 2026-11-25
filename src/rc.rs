// 用 RCGUI 已经连上的 rclone RC 建 teldrive 远程、挂载、卸载。
//
// 为什么走 RC 而不是自己改 rclone.conf：`config/create` 由 rclone 自己落盘并
// 同步内存里的 remote 表，不会和正在运行的实例打架（宿主 AGENTS.md §4 要求
// 优先用 RC 的配置接口）。代价是「连上的那个 rclone」必须是 tgdrive 的 fork，
// 原版会回 `unknown backend "teldrive"` —— 这条错误原样交给用户看，比我们猜
// 一个更好的说法有用。

use crate::i18n::{f, t};
use crate::proto::*;
use serde_json::{json, Value};

fn st_s(kv: &Value, key: &str) -> String {
    s(kv_state(kv).get(key))
}

fn api_key(kv: &Value) -> String {
    st_s(kv, "api_key")
}

fn port_of(kv: &Value) -> u16 {
    kv_state(kv)
        .get("port")
        .and_then(|v| v.as_u64())
        .unwrap_or(8080) as u16
}

fn name(kv: &Value) -> String {
    let n = st_s(kv, "remote");
    if n.is_empty() {
        "teldrive".to_string()
    } else {
        n
    }
}

/// 盘符统一成 `T:` 形式。用户可能填 `t`、`T:`、`T:\`、`t:/`。
fn norm_drive(raw: &str) -> Option<String> {
    let mut cs = raw.trim().chars();
    let c = cs.next()?;
    if !c.is_ascii_alphabetic() {
        return None;
    }
    let rest = cs.as_str().trim();
    if !rest.is_empty() && rest != ":" && rest != ":\\" && rest != ":/" {
        return None;
    }
    Some(format!("{}:", c.to_ascii_uppercase()))
}

pub fn on_remote_create(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            json!({ "step": "go" }),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),

        "go" => {
            let kv = got(&rq.provided, "kv");
            let key = api_key(&kv);
            // API key 只在登录那一步拿得到，而且 teldrive 只显示一次。
            if key.is_empty() {
                return fail(id, &t("remote.need_login"));
            }
            let n = {
                let want = fstr(&rq.form, "name").trim().to_string();
                if want.is_empty() {
                    name(&kv)
                } else {
                    want
                }
            };
            if n.contains(':') || n.contains('/') || n.contains('\\') {
                return fail(id, &t("remote.bad_name"));
            }
            need(
                id,
                json!({ "step": "report", "kv": kv, "name": n }),
                vec![need_item(
                    "rclone.call",
                    "made",
                    json!({
                        "method": "config/create",
                        "params": {
                            "name": n,
                            "type": "teldrive",
                            "parameters": {
                                "api_host": format!("http://127.0.0.1:{}", port_of(&kv)),
                                "api_key": key,
                                "chunk_size": "256M",
                                "upload_concurrency": "2",
                            },
                        },
                    }),
                )],
            )
        }

        "report" => {
            let kv = rq.state.get("kv").cloned().unwrap_or(Value::Null);
            let n = state_s(&rq.state, "name");
            if let Some(e) = got_err(&rq.provided, "made") {
                return fail(id, &f(&t("remote.create_fail"), &[&e]));
            }
            let ops = vec![state_op(&kv, |st| {
                st.insert("remote".into(), json!(n));
            })];
            done_ops(
                id,
                ops,
                json!({
                    "status": f(&t("remote.created"), &[&n]),
                    "lamp": "ok",
                    "values": { "remote": n },
                    "toast": { "level": "info", "message": f(&t("remote.created"), &[&n]) },
                }),
            )
        }

        _ => fail(id, &t("action.unknown")),
    }
}

pub fn on_mount(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            json!({ "step": "go" }),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),

        "go" => {
            let kv = got(&rq.provided, "kv");
            if api_key(&kv).is_empty() {
                return fail(id, &t("remote.need_login"));
            }
            let raw = fstr(&rq.form, "drive");
            let Some(drive) = norm_drive(&raw) else {
                return fail(id, &t("mount.bad_drive"));
            };
            let n = name(&kv);
            // 同一盘符上重复挂载会被 WinFsp 拒绝，先在插件侧挡掉，说法更具体。
            let mounted = st_s(&kv, "mount");
            if !mounted.is_empty() {
                return fail(
                    id,
                    &f(&t("mount.already"), &[&mounted]),
                );
            }
            // 先问一句这个 rclone 有没有挂载实现。Windows 上缺 cmount 标签时
            // mount/mount 会报 "mount option specified is not registered"，那句话
            // 看不出是构建问题，而且它的失败会整轮中断、传不回错误文本，所以只能
            // 提前用 core/version 的 goTags 判，别等报错再猜。
            need(
                id,
                json!({ "step": "ver", "kv": kv, "drive": drive, "remote": n }),
                vec![need_item(
                    "rclone.call",
                    "ver",
                    json!({ "method": "core/version", "params": {} }),
                )],
            )
        }

        "ver" => {
            let kv = rq.state.get("kv").cloned().unwrap_or(Value::Null);
            let drive = state_s(&rq.state, "drive");
            let n = state_s(&rq.state, "remote");
            let tags = s(got(&rq.provided, "ver").get("goTags"));
            if !tags.is_empty() && !tags.contains("cmount") {
                return fail(id, &f(&t("mount.no_cmount"), &[&tags]));
            }
            need(
                id,
                json!({ "step": "report", "kv": kv, "drive": drive, "remote": n }),
                vec![need_item(
                    "rclone.call",
                    "mounted",
                    json!({
                        "method": "mount/mount",
                        "params": {
                            "fs": format!("{}:", n),
                            "mountPoint": drive,
                            // 挂载选项走顶层扁平键（rclone rc 文档：CLI 旗标去掉 `--`、
                            // 连字符换下划线，`volname` 对应 mountOpt 里的 `VolName`）。
                            // 用嵌套 mountOpt 也行，但字段名是 Go 字段名，写错只会收到
                            // 一句看不出所以然的解析错误。
                            "volname": format!("teldrive {}", n),
                        },
                    }),
                )],
            )
        }

        "report" => {
            let kv = rq.state.get("kv").cloned().unwrap_or(Value::Null);
            let drive = state_s(&rq.state, "drive");
            if let Some(e) = got_err(&rq.provided, "mounted") {
                // 这句话是 rclone 在 mountFn == nil 时报的，也就是这个二进制
                // 根本没编译挂载支持（Windows 上缺 cmount 标签就会这样），
                // 与挂载参数无关。原话直译给用户只会把人引向错误方向。
                if e.contains("mount option specified is not registered") {
                    return fail(id, &t("mount.no_cmount"));
                }
                // WinFsp 没装、盘符被占用、后端拒绝都会走到这里，原话最有诊断价值。
                return fail(id, &f(&t("mount.mount_fail"), &[&e]));
            }
            let ops = vec![state_op(&kv, |st| {
                st.insert("mount".into(), json!(drive));
            })];
            done_ops(
                id,
                ops,
                json!({
                    "status": f(&t("mount.done"), &[&drive]),
                    "lamp": "ok",
                    "values": { "drive": drive },
                    "toast": { "level": "info", "message": f(&t("mount.done"), &[&drive]) },
                    "enabled": { "mount": false, "unmount": true },
                }),
            )
        }

        _ => fail(id, &t("action.unknown")),
    }
}

pub fn on_unmount(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            json!({ "step": "go" }),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),

        "go" => {
            let kv = got(&rq.provided, "kv");
            let drive = st_s(&kv, "mount");
            if drive.is_empty() {
                return fail(id, &t("mount.nothing"));
            }
            need(
                id,
                json!({ "step": "report", "kv": kv, "drive": drive }),
                vec![need_item(
                    "rclone.call",
                    "unmounted",
                    json!({
                        "method": "mount/unmount",
                        "params": { "mountPoint": drive },
                    }),
                )],
            )
        }

        "report" => {
            let kv = rq.state.get("kv").cloned().unwrap_or(Value::Null);
            let drive = state_s(&rq.state, "drive");
            if let Some(e) = got_err(&rq.provided, "unmounted") {
                return fail(id, &f(&t("mount.mount_fail"), &[&e]));
            }
            let ops = vec![state_op(&kv, |st| {
                st.insert("mount".into(), Value::Null);
            })];
            done_ops(
                id,
                ops,
                json!({
                    "status": f(&t("mount.undone"), &[&drive]),
                    "lamp": "ok",
                    "values": { "drive": "" },
                    "toast": { "level": "info", "message": f(&t("mount.undone"), &[&drive]) },
                    "enabled": { "mount": true, "unmount": false },
                }),
            )
        }

        _ => fail(id, &t("action.unknown")),
    }
}

//! teldrive 一键部署插件：装服务、登 Telegram、建频道与机器人、建远程、挂载。
//!
//! # 一次调用 = 一个全新进程
//!
//! WASI 里没有进程、没有网络、没有文件对话框，也没有跨轮内存。宿主每次把
//! 「一个 JSON 写到 stdin、读回一个 JSON」当成一次调用，单次最多协商 16 轮。
//! 所以插件每轮都从 `state`（宿主原样回传）和 kv 里重建上下文，能并行的能力
//! 一次问完。
//!
//! # 为什么文件、进程、网络全要「申请」
//!
//! `fs.*` / `http.*` / `proc.*` / `net.*` / `rclone.*` / `env.info` 都由宿主执行，
//! 用到哪几组在 `config.json` 的 `capabilities` 里声明，越权会被直接拒绝。
//! 插件里只剩策略：装哪个版本、装到哪、先看什么、失败说什么。
//!
//! # 面板回包
//!
//! `body` 用到的字段：`status` / `lamp` / `values` / `checks` / `hints` /
//! `enabled` / `progress` / `toast`。其中 `hints`、`enabled` 是按控件 key 索引的
//! 对象，`checks` 是 `{label, value, ok}` 数组。宿主所有 status 控件都只显示
//! `body.status` 这一句话，所以多处状态要自己拼成一句。

mod api;
mod cfg;
mod deploy;
mod i18n;
mod paths;
mod proto;
mod qr;
mod rc;
mod store;
mod svc;

use serde_json::{json, Value};
use std::io::{Read, Write};

use proto::{s, Rq};

fn main() {
    let mut input = Vec::new();
    if std::io::stdin().read_to_end(&mut input).is_err() {
        return;
    }
    let resp = handle(&input).unwrap_or_else(|e| json!({ "id": 1, "error": { "message": e } }));
    let mut out = std::io::stdout();
    let _ = out.write_all(&serde_json::to_vec(&resp).unwrap_or_default());
    let _ = out.flush();
}

fn handle(input: &[u8]) -> Result<Value, String> {
    let req: Value = serde_json::from_slice(input)
        .map_err(|e| i18n::f("req.parse", &[&e.to_string()]))?;
    let id = req.get("id").and_then(|v| v.as_u64()).unwrap_or(1);
    let method = s(req.get("method"));
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    i18n::set_lang(params.get("lang").and_then(|v| v.as_str()));
    match method.as_str() {
        "manifest" => Ok(json!({
            "id": id,
            "manifest": {
                "id": "teldrive",
                "name": i18n::t("plugin.name"),
                "version": env!("CARGO_PKG_VERSION"),
                "description": i18n::t("plugin.desc")
            }
        })),
        "request" => Ok(dispatch(id, &params)),
        other => Err(i18n::f("method.unknown", &[other])),
    }
}

fn dispatch(id: u64, p: &Value) -> Value {
    let action = s(p.get("action"));
    let rq = Rq {
        form: p.get("form").cloned().unwrap_or_else(|| json!({})),
        provided: p.get("provided").cloned().unwrap_or(Value::Null),
        state: p.get("state").cloned().unwrap_or(Value::Null),
    };
    match action.as_str() {
        // 面板打开 / 动作完成后的刷新：只读 kv，不产生副作用。
        "state" => svc::on_state(id, &rq),

        // 部署与本地进程
        "deploy" => deploy::on_deploy(id, &rq),
        "start" => svc::on_start(id, &rq),
        "stop" => svc::on_stop(id, &rq),
        "open_dir" => svc::on_open_dir(id, &rq),

        // Telegram 登录三步：验证码可能要求两步密码，因此拆成两个动作。
        "login_start" => store::on_login_start(id, &rq),
        "login_verify" => store::on_login_verify(id, &rq),
        "login_password" => store::on_login_password(id, &rq),

        // 扫码登录：出码、轮询。两步分开是因为扫码的等待时间比一次调用允许的
        // 要长得多，轮询由用户点「检查扫码结果」触发。
        "qr_start" => store::on_qr_start(id, &rq),
        "qr_poll" => store::on_qr_poll(id, &rq),
        "logout" => store::on_logout(id, &rq),

        // 频道与机器人
        "channel_create" => store::on_channel_create(id, &rq),
        "bot_add" => store::on_bot_add(id, &rq),
        "channels_refresh" => store::on_channels_refresh(id, &rq),
        "channel_use" => store::on_channel_use(id, &rq),
        "bots_refresh" => store::on_bots_refresh(id, &rq),
        "bot_delete" => store::on_bot_delete(id, &rq),

        // 传输代理
        "proxy_apply" => store::on_proxy_apply(id, &rq),

        // 远程与挂载
        "remote_create" => rc::on_remote_create(id, &rq),
        "mount" => rc::on_mount(id, &rq),
        "unmount" => rc::on_unmount(id, &rq),

        // 宿主传来的动作名不在上表：多半是 config.json 与代码不同步，直接说出来。
        other => proto::fail(
            id,
            &i18n::f("action.unknown", &[if other.is_empty() { "(空)" } else { other }]),
        ),
    }
}

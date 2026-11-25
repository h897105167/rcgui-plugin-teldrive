// Telegram 登录、建频道、加机器人。
//
// 插件自己不能发请求，每个按钮都拆成几轮：先申请 http.request，下一轮从
// provided 里读响应。一轮只能 either 申请能力 or 写 kv，所以「拿到响应 → 落盘」
// 要分两轮：响应里的东西先塞进 state 带到下一轮，再和 kv 快照一起合并写回。
//
// 凭证链条，顺序不能颠倒：
//   手机号   → POST /v1/auth/telegram/start           拿 flowId
//   验证码   → POST /v1/auth/telegram/verify-code     拿 accessToken；开了两步验证则返回 202
//   两步密码 → POST /v1/auth/telegram/verify-password 拿 accessToken
//   accessToken（Bearer）→ POST /v1/api-keys          拿 secret，服务端只返回这一次
//   secret 既是 X-Api-Key 凭证，也是 rclone remote 里的 api_key
//
// 建频道服务端也认 X-Api-Key，但加机器人只认 Bearer，而 accessToken 默认只有
// 15 分钟寿命，所以加机器人前发现它没了就先用 refreshToken 换一对新的。
//
// secret 与两个 token 都存在插件自己的 kv 里（`_state`），和 RCGUI 其它本地配置同级。

use crate::api::{self, Auth};
use crate::i18n::{f, t};
use crate::paths::Paths;
use crate::proto::*;
use serde_json::{json, Value};

/// 和 svc.rs 一致：没部署过就没有端口，按默认值拼 URL，让请求自己失败并报错。
const DEFAULT_PORT: u16 = 8080;

/// teldrive 一次最多接受这么多机器人 token（`BotCreateRequest.tokens` maxItems）。
const MAX_BOTS: usize = 200;

fn port_of(kv: &Value) -> u16 {
    kv_state(kv)
        .get("port")
        .and_then(|v| v.as_u64())
        .filter(|p| *p > 0 && *p < 65536)
        .map(|p| p as u16)
        .unwrap_or(DEFAULT_PORT)
}

fn st(kv: &Value, key: &str) -> String {
    s(kv_state(kv).get(key))
}

/// 本次要用的 kv 快照。写回 `_state` 必须基于它合并，否则会抹掉别的字段。
///
/// 刚问到的 kv 在本轮 `provided` 里，早几轮的问到的只在 `state` 里，两种都可能有。
fn kv_of(rq: &Rq) -> Value {
    kv_pick(rq)
}

fn access(kv: &Value) -> String {
    st(kv, "access_token")
}

fn refresh(kv: &Value) -> String {
    st(kv, "refresh_token")
}

fn api_key(kv: &Value) -> String {
    st(kv, "api_key")
}

pub(crate) fn flow_of(kv: &Value) -> (String, bool) {
    let login = kv_state(kv).get("login").cloned().unwrap_or(Value::Null);
    let id = s(login.get("flowId"));
    let need_pw = login
        .get("need_password")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    (id, need_pw)
}

/// 面板刷新时展示的当前状态。secret 本身不显示，只说有没有存到。
///
/// 状态行把「登录 / 频道 / 远程 / 挂载」四件事拼成一句话：宿主里所有 status 控件都只
/// 显示 `body.status` 这一段文字，写成多个 status 控件不会分别显示各自的 value。
fn panel(kv: &Value, note: &str, level: &str) -> Value {
    let logged = !api_key(kv).is_empty();
    let (flow, need_pw) = flow_of(kv);
    let channel = kv_state(kv)
        .get("channel")
        .and_then(|c| c.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let dash = t("store.none");
    let pick = |v: String| if v.is_empty() { dash.clone() } else { v };
    let summary = f(
        &t("store.summary"),
        &[
            &(if logged { t("yes") } else { t("no") }),
            &pick(channel),
            &pick(st(kv, "remote")),
            &pick(st(kv, "mount")),
        ],
    );
    let status = if note.is_empty() {
        summary
    } else {
        format!("{}；{}", note, summary)
    };

    let mut body = json!({
        "status": status,
        "lamp": level,
        "values": {
            "phone": st(kv, "phone"),
            "logged_in": if logged { t("yes") } else { t("no") },
        },
        // 宿主要的是对象：键是 hint 控件的 key，值是这个控件这一轮该显示的文字。
        "hints": {
            "hint_login": t("login.phone_hint"),
            "hint_bot": t("bot.hint"),
        },
        // 登录之后登录入口一律关掉：这时候再点一次只会拿旧凭证去换新 key，
        // 白占一个登录流程，不如让用户先「退出登录」。
        "enabled": {
            "login_start": !logged,
            "qr_start": !logged,
            "qr_poll": !logged && !flow.is_empty() && !need_pw,
            "login_verify": !logged && !flow.is_empty() && !need_pw,
            "login_password": !logged && need_pw,
            "channel_create": logged,
            "bot_add": logged,
            "channels_refresh": logged,
            "channel_use": logged,
            "bots_refresh": logged,
            "logout": logged,
        },
    });
    decorate(kv, &mut body);
    body
}

// ------------------------------------------------- 面板的附加状态（两个 panel 共用）
//
// 频道下拉、机器人清单、登录段的隐藏、扫码轮询这些跟「渲染哪个面板」无关，
// 只跟 kv 有关，所以统一在这里补，store::panel 与 svc::panel 末尾各调一次：
// 用户先看到的是 state 面板，那边漏了同一套隐藏就会前后不一致。

/// 频道下拉的候选文字。同时用于两处：生成候选、把用户选中的文字反查回频道 id。
/// 两边必须出自同一个函数，否则改一下格式就会「选中了但找不到」。
fn channel_label(name: &str, id: &str) -> String {
    f(&t("channel.option"), &[name, id])
}

/// `_state.channels` → (候选项文字, 当前选中项文字)。
///
/// id 存成字符串：Telegram 频道 id 是 int64，负数的频道（超级群）有 13 位以上，
/// 交给 JSON 的 f64 会掉精度。
fn channel_view(kv: &Value) -> (Vec<String>, String) {
    let mut labels = Vec::new();
    let mut current = String::new();
    let st = kv_state(kv);
    let Some(list) = st.get("channels").and_then(|v| v.as_array()) else {
        return (labels, current);
    };
    for c in list {
        let id = s(c.get("id"));
        if id.is_empty() {
            continue;
        }
        let label = channel_label(&s(c.get("name")), &id);
        if c.get("selected").and_then(|v| v.as_bool()).unwrap_or(false) {
            current = label.clone();
        }
        labels.push(label);
    }
    (labels, current)
}

/// `_state.bots_list` → 多行提示文字。没有列表就返回 None（不覆盖控件原有提示）。
fn bots_text(kv: &Value) -> Option<String> {
    let st = kv_state(kv);
    let list = st.get("bots_list").and_then(|v| v.as_array())?;
    let channel = st
        .get("channel")
        .and_then(|c| c.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let scope = if channel.is_empty() {
        t("bot.list_no_channel")
    } else {
        f(&t("bot.list_channel"), &[&channel])
    };
    if list.is_empty() {
        return Some(format!("{}\n{}", t("bot.list_empty"), scope));
    }
    let mut out = vec![
        f(&t("bot.list_head"), &[&list.len().to_string()]),
        scope,
    ];
    for b in list {
        let username = s(b.get("username"));
        let id = s(b.get("id"));
        let who = if username.is_empty() { id } else { username };
        let state = if b.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false) {
            t("bot.list_on")
        } else {
            t("bot.list_off")
        };
        out.push(f(&t("bot.list_line"), &[&who, &state]));
    }
    // 每台机器人一行：宿主那边 hint 是多行文本，换行就是换行。
    Some(out.join("\n"))
}

/// 给任一面板补上「隐藏哪些控件」「要不要自动轮询」「候选/清单」。
pub(crate) fn decorate(kv: &Value, body: &mut Value) {
    let logged = !api_key(kv).is_empty();
    let (flow, need_pw) = flow_of(kv);

    let (labels, current) = channel_view(kv);
    let bot_labels = bot_choices(kv);
    if !labels.is_empty() || !bot_labels.is_empty() {
        let mut choices = serde_json::Map::new();
        if !labels.is_empty() {
            choices.insert("channel_id".into(), json!(labels));
            if let Some(v) = body.get_mut("values").and_then(|v| v.as_object_mut()) {
                v.insert("channel_id".into(), json!(current));
            }
        }
        if !bot_labels.is_empty() {
            choices.insert("bot_id".into(), json!(bot_labels));
        }
        body["choices"] = Value::Object(choices);
    }
    if let Some(text) = bots_text(kv) {
        if let Some(h) = body.get_mut("hints").and_then(|v| v.as_object_mut()) {
            h.insert("hint_bots".into(), json!(text));
        }
    }

    // 登录完就没什么可登的了：整段 ④ 只留「退出登录」。
    // 不给 enabled:false 是因为那是置灰，一排灰按钮照样占着地方、还让人以为坏了。
    if logged {
        body["hide"] = json!([
            "g_login",
            "qr_start",
            "qr_poll",
            "hint_qr",
            "qr",
            "phone",
            "code",
            "password",
            "hint_login",
            "login_start",
            "login_verify",
            "login_password",
        ]);
    }

    // 二维码流程里没有短信验证码这回事：留着那个输入框和「提交验证码」按钮，
    // 点了只会拿到 400。只有短信流程（login 里没有 qr_url）才需要它们。
    let st = kv_state(kv);
    let scanning = !logged
        && !need_pw
        && st
            .get("login")
            .map(|l| !s(l.get("qr_url")).is_empty())
            .unwrap_or(false);
    if scanning {
        body["hide"] = json!(["code", "login_verify"]);
    }

    // 二维码出好了就不用再让用户点「刷新二维码」：手机扫完这边自己发现。
    // 让宿主每秒跑一次 qr_poll，登录成功后会写 api_key，下一轮 panel 不再带 repeat，
    // 宿主随即停掉（set_repeat 见 body 里没有 repeat 就清空）。
    if !logged && !flow.is_empty() && !need_pw {
        body["repeat"] = json!({ "action": "qr_poll", "interval_ms": 2500 });
    }
}

/// 只读请求用哪种凭证：有长期 api_key 就用它，没有才退回 15 分钟的 accessToken。
/// 注意 `/v1/bots` 系列不认 X-Api-Key（OpenAPI 里只有 Bearer 和 Cookie），
/// 机器人相关的请求必须直接发 `Auth::Bearer(access)`，不能走这里。
fn auth_of(kv: &Value) -> Auth {
    let key = api_key(kv);
    if key.is_empty() {
        Auth::Bearer(access(kv))
    } else {
        Auth::ApiKey(key)
    }
}

/// 用 refreshToken 换一对新的 accessToken/refreshToken。
/// `/v1/auth/refresh` 本身不需要鉴权，所以这里发 `Auth::None`。
fn refresh_need(port: u16, rt: &str) -> Value {
    api::call_need_auth(
        "call",
        port,
        "POST",
        "/v1/auth/refresh",
        &Auth::None,
        Some(json!({ "refreshToken": rt })),
        true,
        api::nonce(),
    )
}

/// 手上最新的 refreshToken。刚换过票的那份记在 state 里，比 kv 里的新：
/// 服务端若轮换了 refreshToken，拿 kv 里的旧值再去换必然还是 401。
fn newest_refresh(kv: &Value, rq: &Rq) -> String {
    let st = state_s(&rq.state, "rt");
    if st.is_empty() {
        refresh(kv)
    } else {
        st
    }
}

/// 国际格式：`+` 加 8-15 位数字，首位非 0。服务端的正则就是这个范围。
fn phone_ok(p: &str) -> bool {
    let b = p.as_bytes();
    if b.len() < 9 || b.len() > 16 || b[0] != b'+' || b[1] == b'0' {
        return false;
    }
    b[1..].iter().all(|c| c.is_ascii_digit())
}

/// 机器人 token 一行一个，也容忍逗号和多余空白。
fn tokens_of(raw: &str) -> Vec<String> {
    raw.split(|c: char| c.is_whitespace() || c == ',' || c == ';')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// 把 accessToken 换成一把新的 API key —— 登录成功后的必走一步，不能让用户手动点。
fn key_need(port: u16, access: &str) -> Value {
    api::call_need_auth(
        "call",
        port,
        "POST",
        "/v1/api-keys",
        &Auth::Bearer(access.to_string()),
        Some(json!({ "name": format!("rcgui-{}", port) })),
        true,
        api::nonce(),
    )
}

/// 建 key 那一轮的收尾：把 secret 与两个 token 一起写回，再报成功。
fn finish_login(id: u64, rq: &Rq) -> Value {
    let kv = kv_of(rq);
    if let Some(e) = got_err(&rq.provided, "call") {
        return fail(id, &f("login.key_fail", &[&e]));
    }
    let r = api::reply(&got(&rq.provided, "call"));
    if r.status != 200 && r.status != 201 {
        if r.status == 401 {
            return fail(id, &t("login.expired"));
        }
        return fail(id, &f("login.key_fail", &[&api::err_text(&r)]));
    }
    let secret = s(r.json.get("secret"));
    if secret.is_empty() {
        return fail(id, &f("login.key_fail", &[&t("login.secret_missing")]));
    }
    let at = state_s(&rq.state, "at");
    let rt = state_s(&rq.state, "rt");
    let phone = st(&kv, "phone");
    let op = state_op(&kv, |m| {
        m.insert("api_key".into(), json!(secret));
        m.insert("login".into(), Value::Null);
        if !at.is_empty() {
            m.insert("access_token".into(), json!(at));
        }
        if !rt.is_empty() {
            m.insert("refresh_token".into(), json!(rt));
        }
    });
    let note = f("login.ok", &[&phone]);
    let mut body = panel(&state_view(&op), &note, "ok");
    body["toast"] = json!({ "level": "ok", "message": note });
    done_ops(id, vec![op], body)
}

// ---------------------------------------------------------------- 发送验证码

pub fn on_login_start(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            keep(rq.state.clone(), "step", json!("go")),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        "go" => {
            let kv = kv_of(rq);
            let phone = fstr(&rq.form, "phone");
            if phone.is_empty() {
                return fail(id, &t("login.need_phone"));
            }
            if !phone_ok(&phone) {
                return fail(id, &t("login.phone_bad"));
            }
            let need_http = api::call_need_auth(
                "call",
                port_of(&kv),
                "POST",
                "/v1/auth/telegram/start",
                &Auth::None,
                Some(json!({ "phoneNumber": phone })),
                true,
                api::nonce(),
            );
            let next = keep(
                keep(keep(rq.state.clone(), "step", json!("report")), "kv", kv),
                "phone",
                json!(phone),
            );
            need(id, next, vec![need_http])
        }
        _ => {
            let kv = kv_of(rq);
            if let Some(e) = got_err(&rq.provided, "call") {
                return fail(id, &f("login.fail", &[&e]));
            }
            let r = api::reply(&got(&rq.provided, "call"));
            if r.status != 200 && r.status != 201 {
                return fail(id, &f("login.fail", &[&api::err_text(&r)]));
            }
            let flow_id = s(r.json.get("flowId"));
            if flow_id.is_empty() {
                return fail(id, &f("login.fail", &[&t("login.expired")]));
            }
            let phone = state_s(&rq.state, "phone");
            let expires = s(r.json.get("expiresAt"));
            let note = if expires.is_empty() {
                t("login.code_hint")
            } else {
                f("login.sent_at", &[&expires])
            };
            let op = state_op(&kv, |m| {
                m.insert("phone".into(), json!(phone));
                m.insert(
                    "login".into(),
                    json!({ "flowId": flow_id, "need_password": false }),
                );
            });
            let mut body = panel(&state_view(&op), &note, "ok");
            body["toast"] = json!({ "level": "ok", "message": note });
            done_ops(id, vec![op], body)
        }
    }
}

// ---------------------------------------------------------------- 提交验证码

pub fn on_login_verify(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            keep(rq.state.clone(), "step", json!("go")),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        "go" => {
            let kv = kv_of(rq);
            let code = fstr(&rq.form, "code");
            if code.is_empty() {
                return fail(id, &t("login.need_code"));
            }
            let (flow_id, _) = flow_of(&kv);
            if flow_id.is_empty() {
                return fail(id, &t("login.expired"));
            }
            let need_http = api::call_need_auth(
                "call",
                port_of(&kv),
                "POST",
                "/v1/auth/telegram/verify-code",
                &Auth::None,
                Some(json!({ "flowId": flow_id, "code": code })),
                true,
                api::nonce(),
            );
            let next = keep(keep(rq.state.clone(), "step", json!("code")), "kv", kv);
            need(id, next, vec![need_http])
        }
        "code" => {
            let kv = kv_of(rq);
            if let Some(e) = got_err(&rq.provided, "call") {
                return fail(id, &f("login.fail", &[&e]));
            }
            let r = api::reply(&got(&rq.provided, "call"));
            if r.status == 202 {
                // 202 = 账号开了两步验证：服务端把同一个 flow 发回来，等密码。
                let flow_id = s(r.json.get("flowId"));
                let op = state_op(&kv, |m| {
                    m.insert(
                        "login".into(),
                        json!({ "flowId": flow_id, "need_password": true }),
                    );
                });
                let note = t("login.need_password");
                let mut body = panel(&state_view(&op), &note, "warn");
                body["toast"] = json!({ "level": "warn", "message": note });
                done_ops(id, vec![op], body)
            } else if r.status != 200 {
                if r.status == 410 {
                    return fail(id, &t("login.expired"));
                }
                fail(id, &f("login.fail", &[&api::err_text(&r)]))
            } else {
                let at = api::access_token(&r.json);
                if at.is_empty() {
                    return fail(id, &f("login.fail", &[&t("login.expired")]));
                }
                let rt = s(r.json.get("refreshToken"));
                let need_key = key_need(port_of(&kv), &at);
                let next = keep(
                    keep(keep(rq.state.clone(), "step", json!("key")), "kv", kv),
                    "at",
                    json!(at),
                );
                let next = keep(next, "rt", json!(rt));
                need(id, next, vec![need_key])
            }
        }
        _ => finish_login(id, rq),
    }
}

// ---------------------------------------------------------------- 提交两步密码

pub fn on_login_password(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            keep(rq.state.clone(), "step", json!("go")),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        "go" => {
            let kv = kv_of(rq);
            let pw = fstr(&rq.form, "password");
            if pw.is_empty() {
                return fail(id, &t("login.need_password_body"));
            }
            let (flow_id, _) = flow_of(&kv);
            if flow_id.is_empty() {
                return fail(id, &t("login.expired"));
            }
            let need_http = api::call_need_auth(
                "call",
                port_of(&kv),
                "POST",
                "/v1/auth/telegram/verify-password",
                &Auth::None,
                Some(json!({ "flowId": flow_id, "password": pw })),
                true,
                api::nonce(),
            );
            let next = keep(keep(rq.state.clone(), "step", json!("pw")), "kv", kv);
            need(id, next, vec![need_http])
        }
        "pw" => {
            let kv = kv_of(rq);
            if let Some(e) = got_err(&rq.provided, "call") {
                return fail(id, &f("login.fail", &[&e]));
            }
            let r = api::reply(&got(&rq.provided, "call"));
            if r.status != 200 {
                if r.status == 410 {
                    return fail(id, &t("login.expired"));
                }
                return fail(id, &f("login.fail", &[&api::err_text(&r)]));
            }
            let at = api::access_token(&r.json);
            if at.is_empty() {
                return fail(id, &f("login.fail", &[&t("login.expired")]));
            }
            let rt = s(r.json.get("refreshToken"));
            let need_key = key_need(port_of(&kv), &at);
            let next = keep(
                keep(keep(rq.state.clone(), "step", json!("key")), "kv", kv),
                "at",
                json!(at),
            );
            let next = keep(next, "rt", json!(rt));
            need(id, next, vec![need_key])
        }
        _ => finish_login(id, rq),
    }
}

/// 把二维码图片挂到面板上：宿主 `kind:"image"` 的控件按 key 从 `body.images.<key>` 取绝对路径。
///
/// 只放路径不放字节：面板每次动作都会重算一遍，把几十 KB 的图像塞进 body 是白搬。
/// 文件不存在时宿主自己退化成一条提示行，插件不必预检。
pub(crate) fn with_qr(mut body: Value, p: &Paths) -> Value {
    body["images"] = json!({ "qr": p.qr_png() });
    body
}

// ---------------------------------------------------------------- 扫码登录

/// 扫码登录的第一步：让服务端导出一个登录 token，把二维码画成 PNG 落盘，再让面板显示它。
///
/// 二维码永远写成 `<root>\login-qr.png`（面板的 `kind:"image"` 控件按路径读盘）；
/// 换码时覆盖同一个文件，并把同一个路径再下发一次，宿主就会重新读盘。
pub fn on_qr_start(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            keep(rq.state.clone(), "step", json!("go")),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        "go" => {
            let kv = kv_of(rq);
            let call = api::qr_start_need("call", port_of(&kv), api::nonce());
            let next = keep(keep(rq.state.clone(), "step", json!("flow")), "kv", kv);
            need(id, next, vec![call])
        }
        "flow" => {
            let kv = kv_of(rq);
            if let Some(e) = got_err(&rq.provided, "call") {
                return fail(id, &f("qr.fail", &[&e]));
            }
            let r = api::reply(&got(&rq.provided, "call"));
            if r.status != 200 && r.status != 201 {
                return fail(id, &f("qr.fail", &[&api::err_text(&r)]));
            }
            // 字段名以 teldrive 的 OpenAPI 为准（camelCase），老版本可能用别的写法，两种都试。
            let flow_id = api::pick(&r.json, &["flowId", "id", "flow_id"]);
            let state = api::pick(&r.json, &["state"]);
            let url = api::pick(&r.json, &["qrUrl", "qr_url"]);
            if flow_id.is_empty() {
                return fail(id, &f("qr.fail", &[&api::err_text(&r) ]));
            }
            // 这个账号开了两步验证：服务端不导出二维码，直接要密码。把 flow 存下来，
            // 「提交两步密码」那条路就接得上。
            if url.is_empty() && state == "password_required" {
                let op = state_op(&kv, |m| {
                    m.insert(
                        "login".into(),
                        json!({ "flowId": flow_id, "need_password": true }),
                    );
                });
                let note = t("qr.need_password");
                let mut body = panel(&state_view(&op), &note, "warn");
                body["toast"] = json!({ "level": "warn", "message": note });
                return done_ops(id, vec![op], body);
            }
            if url.is_empty() {
                return fail(id, &f("qr.no_url", &[&api::err_text(&r)]));
            }
            // 连版本 20（约 660 字节）都装不下时 qr::png 给 None：宁可退回手机号登录，
            // 也不要把半个码画出来让用户白扫。
            let Some(png) = crate::qr::png(&url) else {
                return fail(id, &f("qr.too_long", &[&url.len().to_string()]));
            };
            let p = Paths::new(&st(&kv, "root"));
            let next = keep(keep(rq.state.clone(), "step", json!("png")), "kv", kv);
            let next = keep(keep(next, "url", json!(url)), "flow", json!(flow_id));
            need(
                id,
                next,
                vec![need_item(
                    "fs.write_b64",
                    "png",
                    json!({ "path": p.qr_png(), "data_b64": crate::qr::b64(&png) }),
                )],
            )
        }
        _ => {
            let kv = kv_of(rq);
            let p = Paths::new(&st(&kv, "root"));
            if let Some(e) = got_err(&rq.provided, "png") {
                return fail(id, &f("qr.write_fail", &[&e]));
            }
            let flow_id = state_s(&rq.state, "flow");
            let url = state_s(&rq.state, "url");
            let note = t("qr.ready");
            let op = state_op(&kv, |m| {
                m.insert(
                    "login".into(),
                    json!({ "flowId": flow_id, "need_password": false, "qr_url": url }),
                );
            });
            let mut body = with_qr(panel(&state_view(&op), &note, "ok"), &p);
            body["toast"] = json!({ "level": "ok", "message": note });
            done_ops(id, vec![op], body)
        }
    }
}

/// 扫码登录的轮询：手机确认前服务端每轮都回一个新二维码，确认过就回 token。
pub fn on_qr_poll(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            keep(rq.state.clone(), "step", json!("go")),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        "go" => {
            let kv = kv_of(rq);
            let (flow_id, _) = flow_of(&kv);
            if flow_id.is_empty() {
                return fail(id, &t("qr.none"));
            }
            let call = api::qr_poll_need("call", port_of(&kv), &flow_id, api::nonce());
            let next = keep(keep(rq.state.clone(), "step", json!("poll")), "kv", kv);
            need(id, next, vec![call])
        }
        "poll" => {
            let kv = kv_of(rq);
            if let Some(e) = got_err(&rq.provided, "call") {
                return fail(id, &f("qr.fail", &[&e]));
            }
            let r = api::reply(&got(&rq.provided, "call"));
            // 410 = 那个 flow 过期没了；404 = 服务端根本不认这个 flow。
            if r.status == 410 || r.status == 404 {
                return fail(id, &t("qr.expired"));
            }
            // 200 = 手机已确认，体里是 TokenPair；202 = 还没确认，体里是新的二维码。
            if r.status == 200 {
                let at = api::access_token(&r.json);
                if at.is_empty() {
                    return fail(id, &f("qr.fail", &[&api::err_text(&r)]));
                }
                let rt = api::pick(&r.json, &["refreshToken", "refresh_token"]);
                let need_key = key_need(port_of(&kv), &at);
                let next = keep(
                    keep(keep(rq.state.clone(), "step", json!("key")), "kv", kv),
                    "at",
                    json!(at),
                );
                let next = keep(next, "rt", json!(rt));
                return need(id, next, vec![need_key]);
            }
            if r.status != 202 {
                return fail(id, &f("qr.fail", &[&api::err_text(&r)]));
            }
            let (flow_id, _) = flow_of(&kv);
            if api::pick(&r.json, &["state"]) == "password_required" {
                let op = state_op(&kv, |m| {
                    m.insert(
                        "login".into(),
                        json!({ "flowId": flow_id, "need_password": true }),
                    );
                });
                let note = t("qr.need_password");
                let mut body = panel(&state_view(&op), &note, "warn");
                body["toast"] = json!({ "level": "warn", "message": note });
                return done_ops(id, vec![op], body);
            }
            // 还没扫：服务端通常会顺手给一个新链接，那就重画一遍图，让面板换成新码。
            let url = api::pick(&r.json, &["qr_url", "qrUrl"]);
            let png = if url.is_empty() {
                None
            } else {
                crate::qr::png(&url)
            };
            let Some(png) = png else {
                let note = t("qr.waiting");
                let mut body = with_qr(panel(&kv, &note, "warn"), &Paths::new(&st(&kv, "root")));
                body["toast"] = json!({ "level": "warn", "message": note });
                return done(id, body);
            };
            let p = Paths::new(&st(&kv, "root"));
            let next = keep(keep(rq.state.clone(), "step", json!("png")), "kv", kv);
            let next = keep(next, "url", json!(url));
            need(
                id,
                next,
                vec![need_item(
                    "fs.write_b64",
                    "png",
                    json!({ "path": p.qr_png(), "data_b64": crate::qr::b64(&png) }),
                )],
            )
        }
        "png" => {
            let kv = kv_of(rq);
            if let Some(e) = got_err(&rq.provided, "png") {
                return fail(id, &f("qr.write_fail", &[&e]));
            }
            let p = Paths::new(&st(&kv, "root"));
            let (flow_id, _) = flow_of(&kv);
            let url = state_s(&rq.state, "url");
            let op = state_op(&kv, |m| {
                m.insert(
                    "login".into(),
                    json!({ "flowId": flow_id, "need_password": false, "qr_url": url }),
                );
            });
            let note = t("qr.rewritten");
            let mut body = with_qr(panel(&state_view(&op), &note, "warn"), &p);
            body["toast"] = json!({ "level": "warn", "message": note });
            done_ops(id, vec![op], body)
        }
        _ => finish_login(id, rq),
    }
}

// ---------------------------------------------------------------- 退出登录

/// 退出登录：清掉本机的登录凭证，让登录入口重新可用。
///
/// 清的是 `api_key`/`access_token`/`refresh_token`/`login` 四项。频道、远程、挂载
/// 这些已经做完的记录留着 —— 退出一次就看不到自己部署到哪一步，比不清更糟。
/// 已经建好的 rclone remote 里存的是它自己那份 key 的副本，所以远程仍可用。
/// teldrive 没有注销接口，refresh token 到期后自然失效。
pub fn on_logout(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            keep(rq.state.clone(), "step", json!("go")),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        _ => {
            let kv = kv_of(rq);
            let op = state_op(&kv, |m| {
                m.remove("api_key");
                m.insert("access_token".into(), json!(""));
                m.insert("refresh_token".into(), json!(""));
                m.insert("login".into(), Value::Null);
            });
            let note = t("logout.done");
            let mut body = panel(&state_view(&op), &note, "ok");
            body["toast"] = json!({ "level": "ok", "message": note });
            done_ops(id, vec![op], body)
        }
    }
}

// ---------------------------------------------------------------- 建频道

pub fn on_channel_create(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            keep(rq.state.clone(), "step", json!("go")),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        "go" => {
            let kv = kv_of(rq);
            let key = api_key(&kv);
            if key.is_empty() {
                return fail(id, &t("remote.need_login"));
            }
            let name = fstr(&rq.form, "channel_name");
            if name.is_empty() {
                // 服务端允许留空（会按前缀自动命名），但用户得知道自己建了哪个频道，
                // 否则以后「哪个频道在存文件」说不清。
                return fail(id, &t("channel.need_name"));
            }
            let need_http = api::call_need_auth(
                "call",
                port_of(&kv),
                "POST",
                "/v1/channels",
                &Auth::ApiKey(key),
                Some(json!({ "name": name, "selected": true })),
                true,
                api::nonce(),
            );
            let next = keep(keep(rq.state.clone(), "step", json!("report")), "kv", kv);
            need(id, next, vec![need_http])
        }
        // 先列 "channels"：它在 `_` 后面就永远到不了。match 里的顺序就是优先级。
        "channels" => {
            let kv = kv_of(rq);
            let (cid, cname) = (state_s(&rq.state, "cid"), state_s(&rq.state, "cname"));
            let note = state_s(&rq.state, "note");
            let list = got(&rq.provided, "list");
            let op = state_op(&kv, |m| {
                m.insert("channel".into(), json!({ "id": cid, "name": cname }));
                // 拉不到列表就只写频道本身，别把上一次的列表清空——那种情况下旧列表
                // 仍然是对的，比空下拉有用。
                if !list.is_null() {
                    let r = api::reply(&list);
                    if r.status == 200 {
                        insert_channels(m, &r.json);
                    }
                }
            });
            let mut body = panel(&state_view(&op), &note, "ok");
            body["toast"] = json!({ "level": "ok", "message": note });
            done_ops(id, vec![op], body)
        }
        _ => {
            let kv = kv_of(rq);
            if let Some(e) = got_err(&rq.provided, "call") {
                return fail(id, &f("channel.fail", &[&e]));
            }
            let r = api::reply(&got(&rq.provided, "call"));
            if r.status != 200 && r.status != 201 {
                if r.status == 401 {
                    return fail(id, &t("login.expired"));
                }
                return fail(id, &f("channel.fail", &[&api::err_text(&r)]));
            }
            let cid = id_str(r.json.get("id"));
            let cname = s(r.json.get("name"));
            let note = f("channel.created", &[&cname]);
            // 建好的频道要立刻出现在下拉里，所以顺手拉一次列表；列表失败不影响
            // 「频道已经建好了」这个事实，只是下拉暂时是空的。
            let http = channels_need_of(&kv);
            let next = keep(
                keep(
                    keep(keep(rq.state.clone(), "step", json!("channels")), "kv", kv),
                    "cid",
                    json!(cid),
                ),
                "cname",
                json!(cname),
            );
            let next = keep(next, "note", json!(note));
            need(id, next, vec![http])
        }
    }
}

// ---------------------------------------------------------------- 加机器人

pub fn on_bot_add(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            keep(rq.state.clone(), "step", json!("go")),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        "go" => {
            let kv = kv_of(rq);
            let at = access(&kv);
            let rt = refresh(&kv);
            if at.is_empty() && rt.is_empty() {
                return fail(id, &t("login.need_login"));
            }
            let list = tokens_of(&fstr(&rq.form, "tokens"));
            if list.is_empty() {
                return fail(id, &t("bot.need_token"));
            }
            let base = keep(keep(rq.state.clone(), "kv", kv.clone()), "tokens", json!(list));
            if at.is_empty() {
                // accessToken 过期了：先用 refreshToken 换一对新的，再发。
                let need_http = refresh_need(port_of(&kv), &rt);
                let next = keep(base, "step", json!("refresh"));
                need(id, next, vec![need_http])
            } else {
                let need_http = send_bots_need(port_of(&kv), &at, &list);
                let next = keep(base, "step", json!("report"));
                need(id, next, vec![need_http])
            }
        }
        "refresh" => {
            let kv = kv_of(rq);
            if let Some(e) = got_err(&rq.provided, "call") {
                return fail(id, &f("login.fail", &[&e]));
            }
            let r = api::reply(&got(&rq.provided, "call"));
            if r.status != 200 {
                return fail(id, &t("login.reauth"));
            }
            let at = api::access_token(&r.json);
            if at.is_empty() {
                return fail(id, &t("login.reauth"));
            }
            // 服务端不一定换发新的 refreshToken，没给就沿用旧的，别把换票能力丢掉。
            let rt_new = s(r.json.get("refreshToken"));
            let rt = if rt_new.is_empty() {
                newest_refresh(&kv, rq)
            } else {
                rt_new
            };
            let list: Vec<String> = match rq.state.get("tokens") {
                Some(Value::Array(a)) => a.iter().map(|v| s(Some(v))).collect(),
                _ => Vec::new(),
            };
            if list.is_empty() {
                return fail(id, &t("bot.need_token"));
            }
            let need_http = send_bots_need(port_of(&kv), &at, &list);
            let next = keep(
                keep(keep(rq.state.clone(), "step", json!("report")), "kv", kv),
                "at",
                json!(at),
            );
            let next = keep(next, "rt", json!(rt));
            need(id, next, vec![need_http])
        }
        // 先列 "bots"：它在 `_` 后面就永远到不了。match 里的顺序就是优先级。
        "bots" => {
            let kv = kv_of(rq);
            let added = rq
                .state
                .get("added")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let note = state_s(&rq.state, "note");
            let level = state_s(&rq.state, "level");
            let at = state_s(&rq.state, "at");
            let rt = state_s(&rq.state, "rt");
            let list = got(&rq.provided, "list");
            // token 与列表合并成一次 _state 写入：两次 state_op 都从同一份 kv 快照算，
            // 分两次写后一次会盖掉前一次。
            let op = state_op(&kv, |m| {
                m.insert("bots".into(), json!(added));
                if !at.is_empty() {
                    m.insert("access_token".into(), json!(at));
                }
                if !rt.is_empty() {
                    m.insert("refresh_token".into(), json!(rt));
                }
                if !list.is_null() {
                    let r = api::reply(&list);
                    if r.status == 200 {
                        insert_bots(m, &r.json);
                    }
                }
            });
            let mut body = panel(&state_view(&op), &note, &level);
            body["toast"] = json!({ "level": level, "message": note });
            done_ops(id, vec![op], body)
        }
        _ => {
            let kv = kv_of(rq);
            if let Some(e) = got_err(&rq.provided, "call") {
                return fail(id, &f("bot.fail", &[&e]));
            }
            let r = api::reply(&got(&rq.provided, "call"));
            if r.status != 200 && r.status != 201 {
                if r.status == 401 {
                    // accessToken 只活 15 分钟（teldrive `security.access-token-ttl` 的默认值），
                    // 过点之后 `/v1/bots` 一律 401。手上有 refreshToken 就换一对新的再发一次；
                    // 只试一次 —— refreshToken 也过期时再换还是 401，会白打转。
                    // 标记写成字符串 "1"：proto::s 只认 JSON 字符串，写成数字 1 读回来是空，
                    // 守卫等于不存在，这条路径会一直换票换到 16 轮上限。
                    let rt = newest_refresh(&kv, rq);
                    if !rt.is_empty() && state_s(&rq.state, "retried").is_empty() {
                        let next = keep(
                            keep(rq.state.clone(), "step", json!("refresh")),
                            "retried",
                            json!("1"),
                        );
                        return need(id, next, vec![refresh_need(port_of(&kv), &rt)]);
                    }
                    return fail(id, &t("login.reauth"));
                }
                return fail(id, &f("bot.fail", &[&api::err_text(&r)]));
            }
            let added = r
                .json
                .get("bots")
                .and_then(|b| b.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let bad = r
                .json
                .get("failedIndexes")
                .and_then(|b| b.as_array())
                .cloned()
                .unwrap_or_default();

            let mut note = f("bot.added", &[&added.to_string()]);
            let mut level = "ok";
            if !bad.is_empty() {
                let idx: Vec<String> = bad.iter().map(|v| v.to_string()).collect();
                note = format!(
                    "{}；{}",
                    note,
                    f("bot.failed", &[&bad.len().to_string(), &idx.join(", ")])
                );
                level = "warn";
            }
            // 加完顺手拉一次列表，让用户马上看到「现在账号下有哪些机器人」；
            // 列表失败不改结论 —— 机器人确实加上了。这里用刚刷新过的 accessToken
            // （`/v1/bots` 不认 X-Api-Key）。
            let at = state_s(&rq.state, "at");
            let rt = state_s(&rq.state, "rt");
            let http = bots_need_of(&kv, &at);
            let next = keep(
                keep(keep(rq.state.clone(), "step", json!("bots")), "kv", kv),
                "added",
                json!(added),
            );
            let next = keep(keep(next, "note", json!(note)), "level", json!(level));
            let next = keep(keep(next, "at", json!(at)), "rt", json!(rt));
            need(id, next, vec![http])
        }
    }
}

fn send_bots_need(port: u16, access: &str, list: &[String]) -> Value {
    let mut tokens: Vec<String> = list.to_vec();
    // 服务端一次最多 200 个；多的截掉而不是整批拒绝，用户能立刻看到成功的那些。
    tokens.truncate(MAX_BOTS);
    api::call_need_auth(
        "call",
        port,
        "POST",
        "/v1/bots",
        &Auth::Bearer(access.to_string()),
        Some(json!({ "tokens": tokens })),
        true,
        api::nonce(),
    )
}

// ---------------------------------------------------------------- 拉列表 / 切频道

/// 列频道的 need。已登录走 X-Api-Key；登录中途还没换出 secret 时退回 Bearer。
fn channels_need_of(kv: &Value) -> Value {
    api::channels_need("list", port_of(kv), &auth_of(kv))
}

/// 列机器人的 need。`/v1/bots` 只认 Bearer（发 X-Api-Key 会 401），所以这里
/// 用 accessToken，而不能像频道那样走 `auth_of` 的 api_key 优先。
fn bots_need_of(kv: &Value, at: &str) -> Value {
    api::bots_need("list", port_of(kv), &Auth::Bearer(at.to_string()))
}

/// 读 `{items:[…]}` 里的数组；字段缺了当空表，让上层照常提示「还没有」。
fn items_of(v: &Value) -> Vec<Value> {
    v.get("items")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default()
}

/// id 统一转字符串。Telegram 频道 id 是可能为负的 int64，走 JSON number 会被
/// f64 往返掉精度，所以一律当字符串存。
fn id_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn insert_channels(m: &mut serde_json::Map<String, Value>, body: &Value) {
    let list: Vec<Value> = items_of(body)
        .into_iter()
        .filter_map(|c| {
            let id = id_str(c.get("id"));
            if id.is_empty() {
                return None;
            }
            Some(json!({
                "id": id,
                "name": s(c.get("name")),
                "selected": c.get("selected").and_then(|v| v.as_bool()).unwrap_or(false),
            }))
        })
        .collect();
    m.insert("channels".into(), json!(list));
}

fn insert_bots(m: &mut serde_json::Map<String, Value>, body: &Value) {
    let list: Vec<Value> = items_of(body)
        .into_iter()
        .map(|b| {
            json!({
                "id": id_str(b.get("id")),
                "username": s(b.get("username")),
                "enabled": b.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false),
            })
        })
        .collect();
    m.insert("bots_list".into(), json!(list));
}

/// 从 `_state.channels` 按标签反查 (id, name)。标签必须由 `channel_label` 生成：
/// 生成候选与反查各用一种格式的话，选中的就永远对不上。
fn channel_pick(kv: &Value, label: &str) -> Option<(String, String)> {
    let arr = kv_state(kv).get("channels")?.as_array()?.clone();
    arr.iter().find_map(|c| {
        let id = s(c.get("id"));
        if id.is_empty() {
            return None;
        }
        let name = s(c.get("name"));
        (channel_label(&name, &id) == label).then(|| (id, name))
    })
}

fn bot_label(username: &str, id: &str) -> String {
    f(&t("bot.option"), &[username, id])
}

/// `_state.bots_list` → 「删除机器人」下拉的候选文字。
fn bot_choices(kv: &Value) -> Vec<String> {
    let st = kv_state(kv);
    let Some(list) = st.get("bots_list").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|b| {
            let id = id_str(b.get("id"));
            if id.is_empty() {
                return None;
            }
            Some(bot_label(&s(b.get("username")), &id))
        })
        .collect()
}

/// 候选文字 → (id, 用户名)。与 `channel_pick` 同理：标签只有一处生成逻辑。
fn bot_pick(kv: &Value, label: &str) -> Option<(String, String)> {
    let st = kv_state(kv);
    let list = st.get("bots_list")?.as_array()?;
    list.iter().find_map(|b| {
        let id = id_str(b.get("id"));
        if id.is_empty() {
            return None;
        }
        let username = s(b.get("username"));
        (bot_label(&username, &id) == label).then(|| (id, username))
    })
}

pub fn on_channels_refresh(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            keep(rq.state.clone(), "step", json!("go")),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        "go" => {
            let kv = kv_of(rq);
            if api_key(&kv).is_empty() {
                return fail(id, &t("remote.need_login"));
            }
            let http = channels_need_of(&kv);
            let next = keep(keep(rq.state.clone(), "step", json!("report")), "kv", kv);
            need(id, next, vec![http])
        }
        _ => {
            let kv = kv_of(rq);
            if let Some(e) = got_err(&rq.provided, "list") {
                return fail(id, &f("channel.list_fail", &[&e]));
            }
            let r = api::reply(&got(&rq.provided, "list"));
            if r.status == 401 {
                return fail(id, &t("login.expired"));
            }
            if r.status != 200 {
                return fail(id, &f("channel.list_fail", &[&api::err_text(&r)]));
            }
            let op = state_op(&kv, |m| insert_channels(m, &r.json));
            let after = state_view(&op);
            let (labels, _) = channel_view(&after);
            let note = if labels.is_empty() {
                t("channel.list_empty")
            } else {
                f("channel.list_head", &[&labels.len().to_string()])
            };
            let mut body = panel(&after, &note, "ok");
            body["toast"] = json!({ "level": "ok", "message": note });
            done_ops(id, vec![op], body)
        }
    }
}

/// 切换当前频道。下拉里存的是人读的标签，所以先按标签反查 id。
pub fn on_channel_use(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            keep(rq.state.clone(), "step", json!("go")),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        "go" => {
            let kv = kv_of(rq);
            if api_key(&kv).is_empty() {
                return fail(id, &t("remote.need_login"));
            }
            let label = fstr(&rq.form, "channel_id");
            let picked = channel_pick(&kv, &label);
            let Some((cid, cname)) = picked else {
                return fail(id, &t("channel.pick_first"));
            };
            let need_http =
                api::channel_select_need("call", port_of(&kv), &cid, &auth_of(&kv), api::nonce());
            let next = keep(keep(rq.state.clone(), "step", json!("report")), "kv", kv);
            let next = keep(keep(next, "cid", json!(cid)), "cname", json!(cname));
            need(id, next, vec![need_http])
        }
        _ => {
            let kv = kv_of(rq);
            let cid = state_s(&rq.state, "cid");
            let cname = state_s(&rq.state, "cname");
            if let Some(e) = got_err(&rq.provided, "call") {
                return fail(id, &f("channel.select_fail", &[&e]));
            }
            let r = api::reply(&got(&rq.provided, "call"));
            if r.status == 401 {
                return fail(id, &t("login.expired"));
            }
            if r.status != 200 {
                return fail(id, &f("channel.select_fail", &[&api::err_text(&r)]));
            }
            let op = state_op(&kv, |m| {
                m.insert("channel".into(), json!({ "id": cid, "name": cname }));
                if let Some(arr) = m.get_mut("channels").and_then(|v| v.as_array_mut()) {
                    for c in arr.iter_mut() {
                        let hit = s(c.get("id")) == cid;
                        if let Some(o) = c.as_object_mut() {
                            o.insert("selected".into(), json!(hit));
                        }
                    }
                }
            });
            let note = f("channel.updated", &[&cname]);
            let mut body = panel(&state_view(&op), &note, "ok");
            body["toast"] = json!({ "level": "ok", "message": note });
            done_ops(id, vec![op], body)
        }
    }
}

pub fn on_bots_refresh(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            keep(rq.state.clone(), "step", json!("go")),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        "go" => {
            let kv = kv_of(rq);
            let at = access(&kv);
            let rt = refresh(&kv);
            if at.is_empty() && rt.is_empty() {
                return fail(id, &t("login.need_login"));
            }
            let base = keep(keep(rq.state.clone(), "kv", kv.clone()), "step", json!("report"));
            if at.is_empty() {
                // accessToken 已经过期，手上还有 refreshToken：先换一对新的再列。
                need(id, keep(base, "step", json!("refresh")), vec![refresh_need(port_of(&kv), &rt)])
            } else {
                need(id, base, vec![bots_need_of(&kv, &at)])
            }
        }
        "refresh" => {
            let kv = kv_of(rq);
            if let Some(e) = got_err(&rq.provided, "call") {
                return fail(id, &f("login.fail", &[&e]));
            }
            let r = api::reply(&got(&rq.provided, "call"));
            let at = api::access_token(&r.json);
            if r.status != 200 || at.is_empty() {
                return fail(id, &t("login.reauth"));
            }
            // 服务端不一定换发新的 refreshToken，没给就沿用旧的，别把换票能力丢掉。
            let rt_new = s(r.json.get("refreshToken"));
            let rt = if rt_new.is_empty() { refresh(&kv) } else { rt_new };
            let http = bots_need_of(&kv, &at);
            let next = keep(keep(rq.state.clone(), "kv", kv), "step", json!("report"));
            let next = keep(keep(next, "at", json!(at)), "rt", json!(rt));
            need(id, next, vec![http])
        }
        _ => {
            let kv = kv_of(rq);
            if let Some(e) = got_err(&rq.provided, "list") {
                return fail(id, &f("bot.list_fail", &[&e]));
            }
            let r = api::reply(&got(&rq.provided, "list"));
            if r.status == 401 {
                // accessToken 过期：用 refreshToken 换一次再试。只试一次 ——
                // refreshToken 也过期时再换也是 401，会白打转。
                let rt = newest_refresh(&kv, rq);
                if !rt.is_empty() && state_s(&rq.state, "retried").is_empty() {
                    let next = keep(
                        keep(rq.state.clone(), "step", json!("refresh")),
                        "retried",
                        json!("1"),
                    );
                    return need(id, next, vec![refresh_need(port_of(&kv), &rt)]);
                }
                return fail(id, &t("login.reauth"));
            }
            if r.status != 200 {
                return fail(id, &f("bot.list_fail", &[&api::err_text(&r)]));
            }
            let at = state_s(&rq.state, "at");
            let rt = state_s(&rq.state, "rt");
            let op = state_op(&kv, |m| {
                insert_bots(m, &r.json);
                // 这一轮换过票就把新的一对落盘，下次点刷新不必再换。
                if !at.is_empty() {
                    m.insert("access_token".into(), json!(at));
                }
                if !rt.is_empty() {
                    m.insert("refresh_token".into(), json!(rt));
                }
            });
            let after = state_view(&op);
            // state_view 把状态包在 `_state` 之下，所以直接取 after["bots_list"] 永远是 null：
            // 刚拉到的两台机器人也会被说成「还没有机器人」。走 kv_state 才落到那一层。
            let n = kv_state(&after)
                .get("bots_list")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let note = if n == 0 {
                t("bot.list_empty")
            } else {
                f("bot.list_head", &[&n.to_string()])
            };
            let mut body = panel(&after, &note, "ok");
            body["toast"] = json!({ "level": "ok", "message": note });
            done_ops(id, vec![op], body)
        }
    }
}

/// 从 teldrive 的机器人池里删掉一台。Telegram 上那个机器人本身不受影响，
/// 只是在 teldrive 里不再拿它分发上传。
///
/// 这一系列接口只认 Bearer（`X-Api-Key` 会被挡成 401），所以和刷新列表一样
/// 要能换票：accessToken 没了就先换，删的时候 401 再换一次，只重试一次。
pub fn on_bot_delete(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            keep(rq.state.clone(), "step", json!("go")),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),
        "go" => {
            let kv = kv_of(rq);
            let at = access(&kv);
            let rt = refresh(&kv);
            if at.is_empty() && rt.is_empty() {
                return fail(id, &t("login.need_login"));
            }
            let label = fstr(&rq.form, "bot_id");
            let Some((bid, bname)) = bot_pick(&kv, &label) else {
                return fail(id, &t("bot.pick_first"));
            };
            let base = keep(keep(rq.state.clone(), "kv", kv.clone()), "step", json!("del"));
            let base = keep(keep(base, "bid", json!(bid)), "bname", json!(bname));
            if at.is_empty() {
                return need(
                    id,
                    keep(base, "step", json!("refresh")),
                    vec![refresh_need(port_of(&kv), &rt)],
                );
            }
            let http = api::bot_delete_need("del", port_of(&kv), &bid, &Auth::Bearer(at));
            need(id, base, vec![http])
        }
        "refresh" => {
            let kv = kv_of(rq);
            if let Some(e) = got_err(&rq.provided, "call") {
                return fail(id, &f("login.fail", &[&e]));
            }
            let r = api::reply(&got(&rq.provided, "call"));
            let at = api::access_token(&r.json);
            if r.status != 200 || at.is_empty() {
                return fail(id, &t("login.reauth"));
            }
            let rt_new = s(r.json.get("refreshToken"));
            let rt = if rt_new.is_empty() { refresh(&kv) } else { rt_new };
            let bid = state_s(&rq.state, "bid");
            let http = api::bot_delete_need("del", port_of(&kv), &bid, &Auth::Bearer(at.clone()));
            let next = keep(keep(rq.state.clone(), "step", json!("del")), "at", json!(at));
            let next = keep(next, "rt", json!(rt));
            need(id, next, vec![http])
        }
        "list" => {
            let kv = kv_of(rq);
            if let Some(e) = got_err(&rq.provided, "list") {
                return fail(id, &f("bot.list_fail", &[&e]));
            }
            let r = api::reply(&got(&rq.provided, "list"));
            if r.status != 200 {
                return fail(id, &f("bot.list_fail", &[&api::err_text(&r)]));
            }
            let bname = state_s(&rq.state, "bname");
            let at = state_s(&rq.state, "at");
            let rt = state_s(&rq.state, "rt");
            let op = state_op(&kv, |m| {
                insert_bots(m, &r.json);
                if !at.is_empty() {
                    m.insert("access_token".into(), json!(at));
                }
                if !rt.is_empty() {
                    m.insert("refresh_token".into(), json!(rt));
                }
            });
            let note = f("bot.deleted", &[&bname]);
            let mut body = panel(&state_view(&op), &note, "ok");
            body["toast"] = json!({ "level": "ok", "message": note });
            done_ops(id, vec![op], body)
        }
        _ => {
            let kv = kv_of(rq);
            if let Some(e) = got_err(&rq.provided, "del") {
                return fail(id, &f("bot.delete_fail", &[&e]));
            }
            let r = api::reply(&got(&rq.provided, "del"));
            if r.status == 401 {
                let rt = newest_refresh(&kv, rq);
                if !rt.is_empty() && state_s(&rq.state, "retried").is_empty() {
                    let next = keep(
                        keep(rq.state.clone(), "step", json!("refresh")),
                        "retried",
                        json!("1"),
                    );
                    return need(id, next, vec![refresh_need(port_of(&kv), &rt)]);
                }
                return fail(id, &t("login.reauth"));
            }
            // 204 是正常结果（没有 body）。
            if r.status != 204 && r.status != 200 {
                return fail(id, &f("bot.delete_fail", &[&api::err_text(&r)]));
            }
            // 删完重列一遍，而不是本地从数组里摘掉：以服务端返回的列表为准。
            let at = {
                let a = state_s(&rq.state, "at");
                if a.is_empty() {
                    access(&kv)
                } else {
                    a
                }
            };
            let bname = state_s(&rq.state, "bname");
            let next = keep(keep(rq.state.clone(), "step", json!("list")), "bname", json!(bname));
            need(id, next, vec![bots_need_of(&kv, &at)])
        }
    }
}

// ---------------------------------------------------------------- 传输代理

/// 在 config.yaml 的 `telegram:` 块里设置 `proxy`：有这一行就换掉，没有就补一行，
/// 传空值就把这一行删掉。返回整份新文件内容。
///
/// 只按行处理自己生成的那套格式，不引 YAML 依赖。注释、缩进和别的键都原样留着，
/// 用户手改过的值不会被吃掉。
fn with_proxy(text: &str, proxy: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_tg = false;
    let mut touched = false;
    let mut saw_tg = false;

    for raw in text.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        let top = !trimmed.is_empty() && !line.starts_with(' ') && !line.starts_with('\t');
        if top {
            // 走出 telegram 块：块里没写过 proxy 就在这里补上。
            if in_tg && !touched && !proxy.is_empty() {
                out.push(format!("  proxy: \"{proxy}\""));
            }
            in_tg = trimmed.trim_end_matches(':') == "telegram" && !trimmed.starts_with('#');
            if in_tg {
                saw_tg = true;
            }
            out.push(line.to_string());
            continue;
        }
        if in_tg && trimmed.starts_with("proxy:") {
            touched = true;
            if !proxy.is_empty() {
                out.push(format!("  proxy: \"{proxy}\""));
            }
            continue;
        }
        out.push(line.to_string());
    }

    if in_tg && !touched && !proxy.is_empty() {
        out.push(format!("  proxy: \"{proxy}\""));
    }
    // 文件里连 telegram 段都没有（被换成了别的配置），就自己补一段。
    if !saw_tg && !proxy.is_empty() {
        out.push("telegram:".into());
        out.push(format!("  proxy: \"{proxy}\""));
    }

    let mut joined = out.join("\n");
    joined.push('\n');
    joined
}

pub fn on_proxy_apply(id: u64, rq: &Rq) -> Value {
    match state_s(&rq.state, "step").as_str() {
        "" => need(
            id,
            json!({ "step": "go" }),
            vec![need_item("kv.get_all", "kv", json!({}))],
        ),

        "go" => {
            let kv = got(&rq.provided, "kv");
            let root = s(kv_state(&kv).get("root"));
            if root.is_empty() {
                return fail(id, &t("proxy.not_deployed"));
            }
            let proxy = fstr(&rq.form, "proxy").trim().to_string();
            // 空 = 直连；否则必须是我们认识的协议，否则 teldrive 起不来。
            if !proxy.is_empty()
                && !["http://", "https://", "socks5://", "socks5h://"]
                    .iter()
                    .any(|p| proxy.starts_with(p))
            {
                return fail(id, &t("proxy.bad"));
            }
            let p = Paths::new(&root);
            let st = keep(keep(rq.state.clone(), "step", json!("read")), "proxy", json!(proxy));
            let st = keep(st, "kv", kv);
            need(
                id,
                st,
                vec![need_item("fs.read_text", "cfgtext", json!({
                    "path": p.cfg,
                    "max_bytes": 1048576
                }))],
            )
        }

        "read" => {
            if let Some(e) = got_err(&rq.provided, "cfgtext") {
                return fail(id, &f(&t("proxy.read_fail"), &[&e]));
            }
            let kv = kv_of(rq);
            let p = Paths::new(&s(kv_state(&kv).get("root")));
            let proxy = state_s(&rq.state, "proxy");
            let text = with_proxy(&s(got(&rq.provided, "cfgtext").get("text")), &proxy);
            let port = port_of(&kv);
            need(
                id,
                keep(rq.state.clone(), "step", json!("write")),
                vec![
                    need_item("fs.write_text", "w_cfg", json!({ "path": p.cfg, "text": text })),
                    // 同一轮里顺便问端口：决定写完要不要重启服务。
                    need_item("net.port_open", "topen", json!({ "port": port, "timeout_ms": 500 })),
                ],
            )
        }

        "write" => {
            if let Some(e) = got_err(&rq.provided, "w_cfg") {
                return fail(id, &f(&t("proxy.write_fail"), &[&e]));
            }
            let kv = kv_of(rq);
            let proxy = state_s(&rq.state, "proxy");
            let note = if proxy.is_empty() {
                t("proxy.cleared")
            } else {
                f(&t("proxy.saved"), &[&proxy])
            };
            let running = got(&rq.provided, "topen")
                .get("open")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !running {
                return done(id, json!({
                    "status": note,
                    "lamp": "ok",
                    "toast": { "level": "ok", "message": note },
                }));
            }
            let p = Paths::new(&s(kv_state(&kv).get("root")));
            let Some(kill) = crate::svc::stop_need(&kv, &p) else {
                // pid 与句柄都没有，谁都停不掉，只能让用户手点「停止」再「启动」。
                let msg = t("proxy.no_handle");
                return done(id, json!({
                    "status": msg,
                    "lamp": "warn",
                    "toast": { "level": "warn", "message": msg },
                }));
            };
            need(id, keep(rq.state.clone(), "step", json!("start")), vec![kill])
        }

        // 停旧进程那一步的结果不能当判断依据：pid 早就退掉时 taskkill 也返回非 0，
        // 句柄失效同样是报错，而这两种情况下服务其实已经停了。真实状态只看端口。
        "start" => {
            let kv = kv_of(rq);
            need(
                id,
                keep(rq.state.clone(), "step", json!("verify")),
                vec![need_item("net.port_open", "topen", json!({
                    "port": port_of(&kv),
                    "timeout_ms": 1500
                }))],
            )
        }

        "verify" => {
            let kv = kv_of(rq);
            let port = port_of(&kv);
            let open = got(&rq.provided, "topen")
                .get("open")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if open {
                // 新配置已落盘但旧进程还在，起来的仍是旧配置，不能假装成功。
                return fail(id, &f(&t("proxy.stop_fail"), &[&port.to_string()]));
            }
            // 交回启动流程：它自己探库、起服务、写 kv，和用户手点「启动」一样。
            svc_step(id, rq, "")
        }

        // 启动流程的中间轮：动作名还是 proxy_apply，所以用 svc: 前缀认领，
        // 否则它的 go/meta/pgprobe 会落回本函数的同名分支来回打转。
        s if s.starts_with("svc:") => svc_step(id, rq, &s[4..]),

        _ => fail(id, &t("action.unknown")),
    }
}

/// 把一轮交回启动流程（`svc::on_start`），并把返回的 step 前缀成 `svc:`。
///
/// 插件是按动作名分发的：这一轮的动作仍叫 `proxy_apply`，`svc` 给的裸 step
/// （`go`/`meta`/`pgprobe`）会被 `on_proxy_apply` 的同名分支接走，于是写完配置
/// 又回头读配置、停服务、再写一遍，永远不结束。加前缀是为了让下一轮落进
/// `on_proxy_apply` 的 `svc:` 臂，原样转发给启动流程。
///
/// `rq.state` 整个转发（只换 `step`）：启动流程的 `meta`/`pgprobe` 分支要读
/// 上一轮存下的 `kv` 与 `port`。
fn svc_step(id: u64, rq: &Rq, step: &str) -> Value {
    let sub = Rq {
        form: rq.form.clone(),
        provided: rq.provided.clone(),
        state: keep(rq.state.clone(), "step", json!(step)),
    };
    let mut v = crate::svc::on_start(id, &sub);
    if let Some(o) = v.get_mut("state").and_then(|s| s.as_object_mut()) {
        let inner = s(o.get("step"));
        o.insert("step".to_string(), json!(format!("svc:{}", inner)));
    }
    v
}


//! 就地升级 teldrive：只换 `bin\teldrive.exe`，不碰数据库、config.yaml、session、bot token。
//!
//! 顺序是「下载 → 校验 → 备份 → 停服务 → 覆盖 → 用原参数重启 → 健康检查」，任何一步失败都
//! 把备份的 exe 放回去再按原状态处理。三条硬规矩：
//!
//! 1. 校验文件拿不到、或者 zip 的哈希和校验文件里那一行不一致，就什么都不装。
//! 2. 只停本插件这次运行里确实持有句柄的进程。端口有人监听却没有句柄，说明那是别人起的
//!    teldrive，直接拒绝并让用户手动停，绝不按历史 pid 去 taskkill（pid 会被系统复用）。
//! 3. 每一步都把同一个不可变的 [`Plan`] 原样带下去。宿主的 need↔provided 轮数有上限，
//!    升级全程控制在 12 轮以内，所以健康检查用一次 `proc.run` 做有界轮询，而不是每轮一次
//!    HTTP 请求。

use serde_json::{json, Value};

use crate::deploy::TELDRIVE_VER;
use crate::i18n::{f, t};
use crate::paths::Paths;
use crate::proto::*;

/// 官方 release 的最新版信息。只有这个仓库的资产会被认。
pub(crate) const API: &str = "https://api.github.com/repos/tgdrive/teldrive/releases/latest";
/// 官方随包发布的校验文件；没有它就拒绝安装。
pub(crate) const SUMS_ASSET: &str = "teldrive_checksums.txt";
/// 压缩包下载上限。
pub(crate) const MAX_ZIP: u64 = 100 * 1024 * 1024;
/// 校验文件本身很小，读多了没必要。
pub(crate) const MAX_TEXT: u64 = 256 * 1024;
/// 健康检查整段预算：进程刚起来要几十秒才监听端口，这里一次给足。
pub(crate) const HEALTH_TIMEOUT_MS: u64 = 120_000;

struct Rel {
    tag: String,
    ver: String,
    zip: String,
    zip_url: String,
    sums_url: String,
}

/// 升级过程中所有需要跨轮保存的东西。不可变：每轮从 state 读出来、原样写回去。
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Plan {
    pub ver: String,
    pub zip: String,
    pub zip_url: String,
    pub zip_path: String,
    pub sums_url: String,
    /// 校验文件里给这个 zip 的 sha256
    pub sha: String,
    pub unpack: String,
    pub found: String,
    pub stage: String,
    pub bak: String,
    /// 解包出来的那个 exe 的 sha256
    pub source_sha: String,
    /// 复制到暂存位之后重新算的 sha256（必须等于 source_sha）
    pub stage_sha: String,
    /// 备份文件 plan.bak 的 sha256；回滚把 exe 换回来时拿它核对
    pub backup_sha: String,
    pub machine: String,
    pub arch: String,
    pub was_running: bool,
    pub owned: bool,
    pub handle: String,
    pub new_handle: String,
}

impl Plan {
    fn to_value(&self) -> Value {
        json!({
            "ver": self.ver,
            "zip": self.zip,
            "zip_url": self.zip_url,
            "zip_path": self.zip_path,
            "sums_url": self.sums_url,
            "sha": self.sha,
            "unpack": self.unpack,
            "found": self.found,
            "stage": self.stage,
            "bak": self.bak,
            "source_sha": self.source_sha,
            "stage_sha": self.stage_sha,
            "backup_sha": self.backup_sha,
            "machine": self.machine,
            "arch": self.arch,
            "was_running": self.was_running,
            "owned": self.owned,
            "handle": self.handle,
            "new_handle": self.new_handle,
        })
    }

    fn of(v: &Value) -> Plan {
        Plan {
            ver: s(v.get("ver")),
            zip: s(v.get("zip")),
            zip_url: s(v.get("zip_url")),
            zip_path: s(v.get("zip_path")),
            sums_url: s(v.get("sums_url")),
            sha: s(v.get("sha")),
            unpack: s(v.get("unpack")),
            found: s(v.get("found")),
            stage: s(v.get("stage")),
            bak: s(v.get("bak")),
            source_sha: s(v.get("source_sha")),
            stage_sha: s(v.get("stage_sha")),
            backup_sha: s(v.get("backup_sha")),
            machine: s(v.get("machine")),
            arch: s(v.get("arch")),
            was_running: v
                .get("was_running")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
            owned: v.get("owned").and_then(|x| x.as_bool()).unwrap_or(false),
            handle: s(v.get("handle")),
            new_handle: s(v.get("new_handle")),
        }
    }
}

fn state_at(step: &str, kv: &Value, plan: &Plan) -> Value {
    json!({ "step": step, "kv": kv, "plan": plan.to_value() })
}

// ---------------- release 解析 ----------------

fn arch_of(env: &Value) -> String {
    match s(env.get("arch")).to_ascii_lowercase().as_str() {
        "x86_64" | "amd64" => "amd64",
        "aarch64" | "arm64" => "arm64",
        "i686" | "x86" => "x86",
        "arm" => "arm",
        _ => "",
    }
    .to_string()
}

fn release_url(tag: &str, asset: &str) -> String {
    format!("https://github.com/tgdrive/teldrive/releases/download/{tag}/{asset}")
}

/// `None` = 这个 release 没有适合当前架构的 Windows 包。
fn parse_release(body: &Value, arch: &str) -> Result<Option<Rel>, String> {
    if body.get("tag_name").is_none() {
        return Err(t("upd.not_release"));
    }
    let tag = s(body.get("tag_name"));
    let ver = tag.trim_start_matches('v').to_string();
    if ver.is_empty() {
        return Err(t("upd.not_release"));
    }
    let suffix = format!("-windows-{arch}.zip");
    let assets = body
        .get("assets")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default();
    let zip = assets.iter().find_map(|a| {
        let name = s(a.get("name"));
        if name.to_ascii_lowercase().ends_with(&suffix) {
            Some(name)
        } else {
            None
        }
    });
    let Some(zip) = zip else {
        return Ok(None);
    };
    Ok(Some(Rel {
        zip_url: release_url(&tag, &zip),
        sums_url: release_url(&tag, SUMS_ASSET),
        tag,
        ver,
        zip,
    }))
}

/// 校验文件是 `sha256sum` 的格式：`<64 位十六进制>  <文件名>`，文件名前可能多一个 `*`。
fn checksum_of(text: &str, name: &str) -> Option<String> {
    let want = name.trim().to_ascii_lowercase();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        let hash = it.next().unwrap_or("");
        let file = it.next().unwrap_or("").trim_start_matches('*');
        if file.is_empty() || !is_sha256(hash) {
            continue;
        }
        let base = file.rsplit(['/', '\\']).next().unwrap_or(file);
        if base.to_ascii_lowercase() == want {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

fn is_sha256(v: &str) -> bool {
    v.len() == 64 && v.chars().all(|c| c.is_ascii_hexdigit())
}

/// 版本比较：按数字段比，缺段当 0，所以 `2.0.14` > `2.0.9`。
pub(crate) fn ver_gt(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split(['.', '-', '+'])
            .map(|x| x.trim().parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (x, y) = (parse(a), parse(b));
    let n = x.len().max(y.len());
    for i in 0..n {
        let l = *x.get(i).unwrap_or(&0);
        let r = *y.get(i).unwrap_or(&0);
        if l != r {
            return l > r;
        }
    }
    false
}

fn cur_version(kv: &Value) -> String {
    let v = s(kv_state(kv).get("version"));
    if v.is_empty() {
        TELDRIVE_VER.to_string()
    } else {
        v
    }
}

fn paths_of(kv: &Value) -> Paths {
    let root = s(kv_state(kv).get("root"));
    Paths::new(&root)
}

fn port_of(kv: &Value) -> u16 {
    kv_state(kv)
        .get("port")
        .and_then(|v| v.as_u64())
        .filter(|n| *n > 0 && *n < 65_536)
        .map(|n| n as u16)
        .unwrap_or(8080)
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('\\') || dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}\\{name}")
    }
}

/// 健康检查：一次 `proc.run` 里做有界轮询（最多 20 次，每次等 1.5 秒）。
///
/// 为什么放进一个能力调用里：宿主的 need↔provided 有轮数上限，按轮重试会把升级流程顶出上限；
/// PowerShell 循环有明确次数上限，不会无限跑。
fn health_probe_need(port: u16) -> Value {
    let url = format!("http://127.0.0.1:{port}/health/ready");
    let script = format!(
        "$u='{url}'; for($i=0;$i -lt 20;$i++){{ try {{ $r=Invoke-WebRequest -UseBasicParsing \
         -TimeoutSec 4 -Uri $u; if($r.StatusCode -ge 200 -and $r.StatusCode -lt 300){{ Write-Output \
         ('READY ' + $r.StatusCode + ' ' + $r.Content); exit 0 }} else {{ Write-Output ('WAIT ' + \
         $r.StatusCode) }} }} catch {{ $c='000'; try {{ if($_.Exception.Response){{ $c=[string][int]\
         $_.Exception.Response.StatusCode }} }} catch {{}}; Write-Output ('WAIT ' + $c) }}; \
         Start-Sleep -Milliseconds 1500 }}; Write-Output 'TIMEOUT'; exit 1"
    );
    need_item(
        "proc.run",
        "health",
        json!({
            "program": "powershell.exe",
            "args": ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", script],
            "timeout_ms": HEALTH_TIMEOUT_MS,
        }),
    )
}

fn start_need(p: &Paths) -> Value {
    need_item(
        "proc.start",
        "started",
        json!({
            "program": p.teldrive_exe(),
            "args": ["run", "--config", p.cfg],
            "cwd": p.root,
            "silent": true,
            "log": p.log_file,
            "keep_on_exit": true,
        }),
    )
}

fn progress_body(cur: &str, latest: &str, status: String, lamp: &str) -> Value {
    json!({
        "status": status,
        "lamp": lamp,
        "values": { "cur_version": cur, "latest_version": latest },
    })
}

// ---------------- 检查新版本 ----------------

pub fn on_check(id: u64, rq: &Rq) -> Value {
    let step = state_s(&rq.state, "step");
    let kv = kv_pick(rq);
    match step.as_str() {
        "" => {
            let p = paths_of(&kv);
            need(
                id,
                json!({ "step": "api", "kv": kv, "tmp": p.tmp }),
                vec![
                    need_item("env.info", "env", json!({})),
                    need_item("kv.get_all", "kv", json!({})),
                    need_item(
                        "http.text",
                        "api",
                        json!({ "url": API, "timeout_ms": 15000 }),
                    ),
                ],
            )
        }
        "api" => {
            if let Some(e) = got_err(&rq.provided, "api") {
                return fail(id, &f(&t("upd.api_fail"), &[&e]));
            }
            let resp = got(&rq.provided, "api");
            let code = resp.get("status").and_then(|v| v.as_u64()).unwrap_or(0);
            if code != 200 {
                return fail(id, &f(&t("upd.api_http"), &[&code.to_string()]));
            }
            let body = resp
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let arch = arch_of(&got(&rq.provided, "env"));
            let cur = cur_version(&kv);
            let rel = match parse_release(&parsed, &arch) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    let latest = s(parsed.get("tag_name"))
                        .trim_start_matches('v')
                        .to_string();
                    let body = progress_body(
                        &cur,
                        &latest,
                        f(&t("upd.unsupported"), &[&latest, &arch]),
                        "warn",
                    );
                    return done_ops(
                        id,
                        vec![upd_op(&kv, &latest, false, "")],
                        with_enabled(body, false),
                    );
                }
                Err(e) => return fail(id, &e),
            };
            if !ver_gt(&rel.ver, &cur) {
                return done_ops(
                    id,
                    vec![upd_op(&kv, &rel.ver, false, "")],
                    with_enabled(
                        progress_body(&cur, &rel.ver, f(&t("upd.up_to_date"), &[&cur]), "ok"),
                        false,
                    ),
                );
            }
            let tmp = s(rq.state.get("tmp"));
            need(
                id,
                json!({ "step": "sums", "kv": kv, "rel": {
                    "ver": rel.ver, "zip": rel.zip, "sums_url": rel.sums_url, "arch": arch
                }, "tmp": tmp }),
                vec![
                    need_item("fs.mkdir", "mk", json!({ "path": tmp })),
                    need_item(
                        "http.download",
                        "sums",
                        json!({ "url": rel.sums_url, "dest": join(&tmp, SUMS_ASSET),
                                "max_bytes": MAX_TEXT }),
                    ),
                ],
            )
        }
        "sums" => {
            let tmp = s(rq.state.get("tmp"));
            if got_err(&rq.provided, "sums").is_some() {
                let latest = s(rq.state.get("rel").and_then(|r| r.get("ver")));
                let cur = cur_version(&kv);
                let body =
                    progress_body(&cur, &latest, f(&t("upd.no_checksum"), &[&latest]), "warn");
                return done_ops(
                    id,
                    vec![upd_op(&kv, &latest, false, "")],
                    with_enabled(body, false),
                );
            }
            need(
                id,
                json!({ "step": "report", "kv": kv, "rel": rq.state.get("rel").cloned().unwrap_or(Value::Null), "tmp": tmp }),
                vec![need_item(
                    "fs.read_text",
                    "txt",
                    json!({ "path": join(&tmp, SUMS_ASSET), "max_bytes": MAX_TEXT }),
                )],
            )
        }
        "report" => {
            let rel = rq.state.get("rel").cloned().unwrap_or(Value::Null);
            let latest = s(rel.get("ver"));
            let zip = s(rel.get("zip"));
            let cur = cur_version(&kv);
            if let Some(e) = got_err(&rq.provided, "txt") {
                let body = progress_body(
                    &cur,
                    &latest,
                    f(&t("upd.no_checksum"), &[&latest, &e]),
                    "warn",
                );
                return done_ops(
                    id,
                    vec![upd_op(&kv, &latest, false, "")],
                    with_enabled(body, false),
                );
            }
            let text = s(got(&rq.provided, "txt").get("text"));
            match checksum_of(&text, &zip) {
                Some(sum) => {
                    let mut body =
                        progress_body(&cur, &latest, f(&t("upd.have_new"), &[&latest, &cur]), "ok");
                    body["hints"] = json!({ "hint_upd": f(&t("upd.hint.trusted"), &[&latest]) });
                    done_ops(
                        id,
                        vec![upd_op(&kv, &latest, true, &sum)],
                        with_enabled(body, true),
                    )
                }
                None => {
                    let body = progress_body(
                        &cur,
                        &latest,
                        f(&t("upd.no_checksum_line"), &[&zip]),
                        "warn",
                    );
                    done_ops(
                        id,
                        vec![upd_op(&kv, &latest, false, "")],
                        with_enabled(body, false),
                    )
                }
            }
        }
        _ => fail(id, &t("action.unknown")),
    }
}

fn with_enabled(mut body: Value, can_update: bool) -> Value {
    body["enabled"] = json!({ "check_update": true, "update": can_update });
    body
}

/// 记录检查结果。`trusted` 为真才允许升级。
fn upd_op(kv: &Value, latest: &str, trusted: bool, sha: &str) -> Value {
    state_op(kv, |m| {
        m.insert(
            "upd".to_string(),
            json!({ "latest": latest, "trusted": trusted, "sha256": sha }),
        );
    })
}

// ---------------- 执行升级 ----------------

pub fn on_update(id: u64, rq: &Rq) -> Value {
    let step = state_s(&rq.state, "step");
    let kv = kv_pick(rq);
    let mut plan = Plan::of(rq.state.get("plan").unwrap_or(&Value::Null));
    let p = paths_of(&kv);

    match step.as_str() {
        "" => {
            let cur = cur_version(&kv);
            plan.ver = cur.clone();
            need(
                id,
                state_at("api", &kv, &plan),
                vec![
                    need_item("env.info", "env", json!({})),
                    need_item("kv.get_all", "kv", json!({})),
                    need_item(
                        "http.text",
                        "api",
                        json!({ "url": API, "timeout_ms": 15000 }),
                    ),
                ],
            )
        }
        "api" => {
            plan.arch = arch_of(&got(&rq.provided, "env"));
            let cur = cur_version(&kv);
            plan.ver = cur.clone();
            if let Some(e) = got_err(&rq.provided, "api") {
                return fail(id, &f(&t("upd.api_fail"), &[&e]));
            }
            let resp = got(&rq.provided, "api");
            let code = resp.get("status").and_then(|v| v.as_u64()).unwrap_or(0);
            if code != 200 {
                return fail(id, &f(&t("upd.api_http"), &[&code.to_string()]));
            }
            let body = resp
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let rel = match parse_release(&parsed, &plan.arch) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    let latest = s(parsed.get("tag_name"))
                        .trim_start_matches('v')
                        .to_string();
                    return done_ops(
                        id,
                        vec![upd_op(&kv, &latest, false, "")],
                        with_enabled(
                            progress_body(
                                &cur,
                                &latest,
                                f(&t("upd.unsupported"), &[&latest, &plan.arch]),
                                "warn",
                            ),
                            false,
                        ),
                    );
                }
                Err(e) => return fail(id, &e),
            };
            let tag = rel.tag.clone();
            if !ver_gt(&rel.ver, &cur) {
                return done_ops(
                    id,
                    vec![upd_op(&kv, &rel.ver, false, "")],
                    with_enabled(
                        progress_body(&cur, &rel.ver, f(&t("upd.up_to_date"), &[&cur]), "ok"),
                        false,
                    ),
                );
            }
            // 不要求先点过「检查新版本」：升级自己下载校验文件，自己比对。
            let _ = tag;
            plan.zip = rel.zip.clone();
            plan.zip_url = rel.zip_url.clone();
            plan.sums_url = rel.sums_url.clone();
            plan.zip_path = join(&p.tmp, &rel.zip);
            plan.unpack = join(&p.tmp, "unpack");
            plan.stage = join(&p.bin, "teldrive.new.exe");
            plan.bak = join(&p.bin, "teldrive.exe.bak");
            plan.ver = rel.ver.clone();
            plan.sha = String::new();
            need(
                id,
                state_at("dl", &kv, &plan),
                vec![
                    need_item("fs.mkdir", "mk", json!({ "path": p.tmp })),
                    need_item("fs.remove", "clean", json!({ "path": plan.unpack })),
                    need_item(
                        "http.download",
                        "zip",
                        json!({ "url": plan.zip_url, "dest": plan.zip_path, "max_bytes": MAX_ZIP }),
                    ),
                    need_item(
                        "http.download",
                        "sums",
                        json!({ "url": plan.sums_url, "dest": join(&p.tmp, SUMS_ASSET),
                                "max_bytes": MAX_TEXT }),
                    ),
                ],
            )
        }
        "dl" => {
            if let Some(e) = got_err(&rq.provided, "zip") {
                return fail(id, &f(&t("upd.dl_fail"), &[&e]));
            }
            if let Some(e) = got_err(&rq.provided, "sums") {
                return fail(id, &f(&t("upd.no_checksum"), &[&plan.ver, &e]));
            }
            let got_zip = got(&rq.provided, "zip");
            let real = s(got_zip.get("sha256")).to_ascii_lowercase();
            if !is_sha256(&real) {
                // 宿主没给出哈希就没法证明文件是好的，不猜。
                return fail(id, &f(&t("upd.hash_empty"), &[&plan.zip]));
            }
            plan.sha = real;
            need(
                id,
                state_at("verify", &kv, &plan),
                vec![need_item(
                    "fs.read_text",
                    "txt",
                    json!({ "path": join(&p.tmp, SUMS_ASSET), "max_bytes": MAX_TEXT }),
                )],
            )
        }
        "verify" => {
            if let Some(e) = got_err(&rq.provided, "txt") {
                return fail(id, &f(&t("upd.no_checksum"), &[&plan.ver, &e]));
            }
            let text = s(got(&rq.provided, "txt").get("text"));
            match checksum_of(&text, &plan.zip) {
                None => return fail(id, &f(&t("upd.no_checksum_line"), &[&plan.zip])),
                Some(sum) if sum != plan.sha => {
                    // 下载到的东西和校验文件里写的不是同一个文件：停在这里，exe 一动没动。
                    return fail(
                        id,
                        &f(&t("upd.sum_mismatch"), &[&plan.zip, &sum, &plan.sha]),
                    );
                }
                Some(_) => {}
            }
            need(
                id,
                state_at("unzip", &kv, &plan),
                vec![need_item(
                    "fs.unzip",
                    "unz",
                    json!({ "zip": plan.zip_path, "dest": plan.unpack, "max_bytes": MAX_ZIP }),
                )],
            )
        }
        "unzip" => {
            if let Some(e) = got_err(&rq.provided, "unz") {
                return fail(id, &f(&t("upd.unzip_fail"), &[&e]));
            }
            need(
                id,
                state_at("found", &kv, &plan),
                vec![need_item(
                    "fs.find",
                    "find",
                    json!({ "dir": plan.unpack, "name": "teldrive.exe", "max_depth": 5 }),
                )],
            )
        }
        "found" => {
            let find = got(&rq.provided, "find");
            let found = s(find.get("path"));
            if found.is_empty() {
                return fail(id, &t("upd.no_exe"));
            }
            plan.found = found.clone();
            need(
                id,
                state_at("src", &kv, &plan),
                vec![
                    need_item("fs.sha256", "sha", json!({ "path": found })),
                    need_item("fs.pe_info", "pe", json!({ "path": found })),
                    need_item("fs.copy", "cp", json!({ "from": found, "to": plan.stage })),
                ],
            )
        }
        "src" => {
            if let Some(e) = got_err(&rq.provided, "cp") {
                return fail(id, &f(&t("upd.stage_fail"), &[&e]));
            }
            plan.source_sha = s(got(&rq.provided, "sha").get("sha256")).to_ascii_lowercase();
            if !is_sha256(&plan.source_sha) {
                return fail(id, &f(&t("upd.hash_empty"), &[&plan.found]));
            }
            let pe = got(&rq.provided, "pe");
            if pe.get("ok").and_then(|v| v.as_bool()) != Some(true) {
                return fail(id, &f(&t("upd.no_machine"), &[&s(pe.get("error"))]));
            }
            plan.machine = s(pe.get("machine")).to_ascii_lowercase();
            if plan.machine.is_empty() {
                return fail(id, &f(&t("upd.no_machine"), &[&plan.found]));
            }
            if plan.machine != plan.arch {
                return fail(
                    id,
                    &f(&t("upd.arch_mismatch"), &[&plan.machine, &plan.arch]),
                );
            }
            let handle = s(kv_state(&kv).get("proc").and_then(|v| v.get("handle")));
            plan.handle = handle.clone();
            need(
                id,
                state_at("stage", &kv, &plan),
                vec![
                    need_item("fs.sha256", "stsha", json!({ "path": plan.stage })),
                    need_item(
                        "fs.copy",
                        "bak",
                        json!({ "from": p.teldrive_exe(), "to": plan.bak }),
                    ),
                    need_item("fs.sha256", "baksha", json!({ "path": plan.bak })),
                    need_item("proc.status", "own", json!({ "handle": handle })),
                    need_item(
                        "net.port_open",
                        "port",
                        json!({ "port": port_of(&kv), "timeout_ms": 800 }),
                    ),
                ],
            )
        }
        "stage" => {
            plan.stage_sha = s(got(&rq.provided, "stsha").get("sha256")).to_ascii_lowercase();
            if !is_sha256(&plan.stage_sha) {
                return fail(id, &f(&t("upd.hash_empty"), &[&plan.stage]));
            }
            if plan.stage_sha != plan.source_sha {
                return fail(
                    id,
                    &f(&t("upd.stage_bad"), &[&plan.source_sha, &plan.stage_sha]),
                );
            }
            if let Some(e) = got_err(&rq.provided, "bak") {
                return fail(id, &f(&t("upd.bak_fail"), &[&e]));
            }
            // 备份文件的哈希：回滚把旧 exe 换回来之后要拿它核对，所以现在就记下来。
            plan.backup_sha = s(got(&rq.provided, "baksha").get("sha256")).to_ascii_lowercase();
            if !is_sha256(&plan.backup_sha) {
                return fail(id, &f(&t("upd.bak_hash_bad"), &[&plan.bak]));
            }
            let own = got(&rq.provided, "own");
            let running = own.get("running").and_then(|v| v.as_bool()) == Some(true);
            let open = got(&rq.provided, "port")
                .get("open")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            plan.owned = !plan.handle.is_empty() && running;
            plan.was_running = running || open;
            if open && !plan.owned {
                // 端口有人监听，但本机没有我们这次的句柄：那是别人的进程，不碰。
                return fail(id, &f(&t("upd.external"), &[&port_of(&kv).to_string()]));
            }
            if plan.owned {
                need(
                    id,
                    state_at("stopped", &kv, &plan),
                    vec![need_item(
                        "proc.stop",
                        "stop",
                        json!({ "handle": plan.handle }),
                    )],
                )
            } else {
                replace_need(id, &kv, &plan)
            }
        }
        "stopped" => {
            if let Some(e) = got_err(&rq.provided, "stop") {
                return fail(id, &f(&t("upd.stop_fail"), &[&e]));
            }
            let stopped = got(&rq.provided, "stop")
                .get("stopped")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !stopped {
                return fail(id, &f(&t("upd.stop_fail"), &[&t("upd.stop_unknown")]));
            }
            replace_need(id, &kv, &plan)
        }
        "swapped" => {
            if let Some(e) = got_err(&rq.provided, "swap") {
                plan.stage_sha = e;
                return rollback(id, &kv, &plan, "upd.swap_fail", false);
            }
            let exe_sha = s(got(&rq.provided, "exesha").get("sha256")).to_ascii_lowercase();
            if exe_sha != plan.source_sha {
                return rollback(id, &kv, &plan, "upd.stage_bad", false);
            }
            plan.stage_sha = exe_sha;
            // 升级前是停的，也先起一次做健康检查，检查完再停回去（见 cleanstop）。
            need(id, state_at("started", &kv, &plan), vec![start_need(&p)])
        }
        "started" => {
            if let Some(e) = got_err(&rq.provided, "started") {
                plan.stage_sha = e;
                return rollback(id, &kv, &plan, "upd.start_fail", false);
            }
            plan.new_handle = s(got(&rq.provided, "started").get("handle"));
            if plan.new_handle.is_empty() {
                // exe 已经换掉了，但拿不到句柄：既停不掉新进程，也不能断定端口上是它。
                // 这时按「回滚」处理会再起一份旧进程跟它抢端口，所以只如实报告。
                return started_unknown(id, &plan);
            }
            need(
                id,
                state_at("health", &kv, &plan),
                vec![
                    need_item("proc.status", "alive", json!({ "handle": plan.new_handle })),
                    health_probe_need(port_of(&kv)),
                ],
            )
        }
        "health" => {
            // 先确认刚起来的子进程还活着：它要是立刻崩了，端口上应答的可能是别人。
            let alive = got(&rq.provided, "alive")
                .get("running")
                .and_then(|v| v.as_bool());
            if alive != Some(true) {
                plan.stage_sha = t("upd.start_died");
                return rollback(id, &kv, &plan, "upd.start_fail", false);
            }
            let run = got(&rq.provided, "health");
            let code = run.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
            let out = s(run.get("stdout"));
            let err = s(run.get("stderr"));
            let ready = code == 0 && out.contains("READY");
            if !ready {
                let why = if out.trim().is_empty() { err } else { out };
                plan.stage_sha = why.clone();
                return rollback(id, &kv, &plan, "upd.health_fail", true);
            }
            if plan.was_running {
                cleanup_need(id, &kv, &plan)
            } else {
                need(
                    id,
                    state_at("cleanstop", &kv, &plan),
                    vec![need_item(
                        "proc.stop",
                        "stopnew",
                        json!({ "handle": plan.new_handle }),
                    )],
                )
            }
        }
        "cleanstop" => {
            // 升级前服务是停的，健康检查后又停回去。proc.stop 会正常返回
            // {stopped:false}（比如进程自己退了又复生），所以不能只看有没有报错。
            let ok = got_err(&rq.provided, "stopnew").is_none()
                && got(&rq.provided, "stopnew")
                    .get("stopped")
                    .and_then(|v| v.as_bool())
                    == Some(true);
            cleanup_need_with(
                id,
                &kv,
                &plan,
                if ok { None } else { Some("upd.stop_restore") },
            )
        }
        "clean" => {
            // 正常路径上 stage_sha 放的是 exe 的哈希；放别的说明前面的步骤留了警告
            // （升级前是停的，但新进程没停回去）。警告必须露出来，不能当成成功。
            let warn = if plan.stage_sha.is_empty() || is_sha256(&plan.stage_sha) {
                String::new()
            } else {
                plan.stage_sha.clone()
            };
            let (text, lamp) = if warn.is_empty() {
                let text = if plan.was_running {
                    f(&t("upd.done"), &[&plan.ver])
                } else {
                    f(&t("upd.done_stopped"), &[&plan.ver])
                };
                (text, "ok")
            } else {
                (f(&t("upd.done_warn"), &[&plan.ver, &warn]), "warn")
            };
            let mut body = progress_body(&plan.ver, &plan.ver, text, lamp);
            if plan.zip_path.is_empty() {
                body["hints"] = json!({ "hint_upd": t("upd.hint.trusted") });
            }
            let ver = plan.ver.clone();
            let handle = plan.new_handle.clone();
            // 新进程要是没停掉就不能把句柄丢掉：否则它还活着，插件却认不出来了。
            let running = plan.was_running || !warn.is_empty();
            let kv2 = kv.clone();
            let ops = vec![state_op(&kv2, move |m| {
                m.insert("version".to_string(), json!(ver));
                if running {
                    m.insert("proc".to_string(), json!({ "handle": handle }));
                } else {
                    m.insert("proc".to_string(), Value::Null);
                }
                m.insert("upd".to_string(), json!({}));
            })];
            done_ops(id, ops, body)
        }

        // ---------------- 回滚：逐轮检查每一步的结果 ----------------
        "rb_stop" => {
            let r = got(&rq.provided, "rb1");
            let stopped = got_err(&rq.provided, "rb1").is_none()
                && r.get("stopped").and_then(|v| v.as_bool()) == Some(true);
            if !stopped {
                // 新进程还在跑：这时候覆盖 exe 会写出半个文件，也没有东西可恢复。
                return rb_failed(id, &plan, "upd.rolled_broken_stop");
            }
            need(
                id,
                state_at("rb_copy", &kv, &plan),
                vec![
                    need_item(
                        "fs.copy",
                        "rb2",
                        json!({ "from": plan.bak, "to": p.teldrive_exe() }),
                    ),
                    need_item("fs.sha256", "rb2sha", json!({ "path": p.teldrive_exe() })),
                ],
            )
        }
        "rb_copy" => {
            let back = s(got(&rq.provided, "rb2sha").get("sha256")).to_ascii_lowercase();
            if got_err(&rq.provided, "rb2").is_some() || !is_sha256(&back) {
                return rb_failed(id, &plan, "upd.rolled_broken_restore");
            }
            if !is_sha256(&plan.backup_sha) || back != plan.backup_sha {
                // 写回去的东西跟备份不是同一个文件：不能算恢复成功。
                plan.stage_sha = f(&t("upd.rb_hash_detail"), &[&plan.backup_sha, &back]);
                return rb_failed(id, &plan, "upd.rolled_broken_hash");
            }
            if plan.was_running {
                need(id, state_at("rb_start", &kv, &plan), vec![start_need(&p)])
            } else {
                finalize_rollback(id, &kv, &plan, true)
            }
        }
        "rb_start" => {
            if let Some(e) = got_err(&rq.provided, "started") {
                plan.stage_sha = e;
                return rb_failed(id, &plan, "upd.rolled_broken_start");
            }
            let h = s(got(&rq.provided, "started").get("handle"));
            if h.is_empty() {
                // 启起来了却拿不到句柄：不能说「已恢复运行」，也记不下能管它的句柄。
                plan.stage_sha = t("upd.rb_nohandle");
                return rb_failed(id, &plan, "upd.rolled_broken_start");
            }
            plan.new_handle = h;
            finalize_rollback(id, &kv, &plan, true)
        }
        _ => fail(id, &t("action.unknown")),
    }
}

/// 覆盖 exe 并重新算哈希核对（写文件和算哈希放同一轮）。
fn replace_need(id: u64, kv: &Value, plan: &Plan) -> Value {
    let p = paths_of(kv);
    need(
        id,
        state_at("swapped", kv, plan),
        vec![
            need_item(
                "fs.copy",
                "swap",
                json!({ "from": plan.stage, "to": p.teldrive_exe() }),
            ),
            need_item("fs.sha256", "exesha", json!({ "path": p.teldrive_exe() })),
        ],
    )
}

fn cleanup_need(id: u64, kv: &Value, plan: &Plan) -> Value {
    cleanup_need_with(id, kv, plan, None)
}

/// 删掉下载的 zip 和解包目录。删不掉不算升级失败，下轮升级开始时还会再清一次。
fn cleanup_need_with(id: u64, kv: &Value, plan: &Plan, warn: Option<&str>) -> Value {
    let mut plan2 = plan.clone();
    plan2.stage_sha = warn.map(|k| t(k)).unwrap_or_default();
    need(
        id,
        state_at("clean", kv, &plan2),
        vec![
            need_item("fs.remove", "rmzip", json!({ "path": plan.zip_path })),
            need_item("fs.remove", "rmunz", json!({ "path": plan.unpack })),
        ],
    )
}

fn rollback(id: u64, kv: &Value, plan: &Plan, key: &str, new_running: bool) -> Value {
    let mut plan2 = plan.clone();
    plan2.stage_sha = f(&t(key), &[&plan.stage_sha]);
    plan2.owned = new_running;
    if new_running && !plan.new_handle.is_empty() {
        need(
            id,
            state_at("rb_stop", kv, &plan2),
            vec![need_item(
                "proc.stop",
                "rb1",
                json!({ "handle": plan.new_handle }),
            )],
        )
    } else {
        let p = paths_of(kv);
        need(
            id,
            state_at("rb_copy", kv, &plan2),
            vec![
                need_item(
                    "fs.copy",
                    "rb2",
                    json!({ "from": plan.bak, "to": p.teldrive_exe() }),
                ),
                need_item("fs.sha256", "rb2sha", json!({ "path": p.teldrive_exe() })),
            ],
        )
    }
}

fn rb_failed(id: u64, plan: &Plan, key: &str) -> Value {
    let mut body = progress_body(&plan.ver, &plan.ver, f(&t(key), &[&plan.stage_sha]), "err");
    body["hints"] = json!({ "hint_upd": t("upd.rolled_broken_hint") });
    done(id, body)
}

/// 新 exe 已经落到盘上，但宿主没给出新进程的句柄。
///
/// 这种时候既不能假装失败得上一个「已恢复」的结论（恢复旧 exe 再起一次，会和
/// 那个还活着、却认不出来的新进程同时抢端口），也不能替用户拍板停掉它。所以
/// 什么都不做：不动 kv、不发任何进程动作，把现状和该查什么写进结果。
fn started_unknown(id: u64, plan: &Plan) -> Value {
    let mut body = progress_body(
        &plan.ver,
        &plan.ver,
        f(&t("upd.started_unknown"), &[&plan.ver]),
        "err",
    );
    body["hints"] = json!({ "hint_upd": t("upd.rolled_broken_hint") });
    done(id, body)
}

fn finalize_rollback(id: u64, kv: &Value, plan: &Plan, started: bool) -> Value {
    let (_cur, _latest) = (plan.ver.clone(), plan.ver.clone());
    let status = if plan.was_running && started {
        f(&t("upd.rolled_running"), &[&plan.stage_sha])
    } else {
        f(&t("upd.rolled_stopped"), &[&plan.stage_sha])
    };
    let mut body = progress_body(&plan.ver, &plan.ver, status, "err");
    body["hints"] = json!({ "hint_upd": t("upd.rolled_hint") });
    // 版本号保持原样：升级没成，kv 里不该出现新版本。
    //
    // 旧进程要是已经用原参数起来了，句柄必须留在 kv 里：进程活着而插件认不出来，
    // 之后用户点停止会被拒，再次升级也会以为「端口上的不是自己的」而拒绝。
    // 没重启成功（或本来就没在跑）时不要动 proc，别把仍然有效的旧句柄抹掉。
    let keep = if plan.was_running && started && !plan.new_handle.is_empty() {
        Some(plan.new_handle.clone())
    } else {
        None
    };
    let kv2 = kv.clone();
    let ops = vec![state_op(&kv2, move |m| {
        if let Some(h) = keep {
            m.insert("proc".to_string(), json!({ "handle": h }));
        }
        m.insert("upd".to_string(), json!({}));
    })];
    done_ops(id, ops, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Rq;
    use serde_json::Map;

    const NEW_VER: &str = "9.9.9";
    const SUM: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const EXE: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    fn release_json(name: &str, tag: &str) -> String {
        json!({
            "tag_name": tag,
            "assets": [
                { "name": "teldrive_checksums.txt" },
                { "name": name },
            ]
        })
        .to_string()
    }

    /// 一个假的宿主：按 need 里的 cap + params 造结果，并记下调用序列。
    struct Fake {
        kv: Value,
        zip_name: String,
        /// 校验文件内容
        sums: String,
        /// 下载回来的 zip 真实哈希
        zip_sha: String,
        machine: String,
        /// 解包出来的 exe 的哈希
        source_sha: String,
        /// 复制到暂存位之后算出来的哈希（默认等于 source_sha）
        stage_sha: String,
        /// 覆盖之后 exe 的哈希（默认等于 source_sha）
        exe_sha: String,
        /// 备份文件 teldrive.exe.bak 的哈希（默认等于 source_sha）
        bak_sha: String,
        port_open: bool,
        running: bool,
        /// 这个宿主发出去过的进程句柄。真实宿主只认自己发过的句柄，别的都不认。
        handles: Vec<String>,
        start_fails: bool,
        /// 起得来但宿主不给句柄（真实宿主可能这样：进程跑起来了，身份没拿到）
        start_nohandle: bool,
        /// 停的时候返回 {stopped:false}（进程自己退了又复生之类）
        stop_stopped: bool,
        /// 前几次 proc.stop 允许成功；超过就回 {stopped:false}。
        /// 用来区分「原进程停不掉」和「回滚时新进程停不掉」。
        stop_true_times: usize,
        /// 本次 proc.start 铸出来的句柄：它对应的进程是活的，跟升级前在不在跑无关
        started_handle: Option<String>,
        health_ok: bool,
        copy_fails: Vec<String>,
        calls: Vec<String>,
        states: Vec<Value>,
    }

    impl Fake {
        fn new() -> Fake {
            Fake {
                kv: json!({ "root": "C:\\data\\plugins\\teldrive", "port": 8080,
                            "proc": { "handle": "teldrive#1" }, "version": "2.0.0" }),
                zip_name: format!("teldrive-{NEW_VER}-windows-amd64.zip"),
                sums: format!(
                    "{SUM}  teldrive-{NEW_VER}-windows-amd64.zip\n3333  teldrive-{NEW_VER}-windows-arm64.zip\n"
                ),
                zip_sha: SUM.to_string(),
                machine: "amd64".to_string(),
                source_sha: EXE.to_string(),
                stage_sha: EXE.to_string(),
                exe_sha: EXE.to_string(),
                bak_sha: EXE.to_string(),
                port_open: false,
                running: true,
                handles: vec!["teldrive#1".to_string()],
                start_fails: false,
                start_nohandle: false,
                stop_stopped: true,
                stop_true_times: usize::MAX,
                started_handle: None,
                health_ok: true,
                copy_fails: vec![],
                calls: vec![],
                states: vec![],
            }
        }

        fn err(msg: &str) -> Value {
            json!({ "_error": msg })
        }

        fn answer(&mut self, cap: &str, params: &Value) -> Value {
            let path = s(params.get("path"));
            let from = s(params.get("from"));
            let to = s(params.get("to"));
            match cap {
                "env.info" => json!({ "arch": "x86_64", "data_dir": "C:\\data" }),
                "kv.get_all" => json!({ "_state": self.kv }),
                "http.text" => json!({
                    "status": 200,
                    "body": release_json(&self.zip_name, &format!("v{NEW_VER}")),
                }),
                "fs.mkdir" | "fs.remove" => json!({ "ok": true }),
                "http.download" => {
                    let url = s(params.get("url"));
                    if url.ends_with(SUMS_ASSET) {
                        json!({ "path": path, "bytes": 10, "sha256": "4444" })
                    } else {
                        json!({ "path": path, "bytes": 27, "sha256": self.zip_sha })
                    }
                }
                "fs.read_text" => json!({ "text": self.sums, "size": self.sums.len() }),
                "fs.unzip" => json!({ "files": 3, "dest": path }),
                "fs.find" => {
                    json!({ "path": format!("{}\\teldrive.exe", s(params.get("dir"))), "found": true })
                }
                "fs.sha256" => {
                    let sha = if path.ends_with("teldrive.new.exe") {
                        self.stage_sha.clone()
                    } else if path.ends_with("teldrive.exe.bak") {
                        self.bak_sha.clone()
                    } else if path.ends_with("teldrive.exe") {
                        self.exe_sha.clone()
                    } else {
                        self.source_sha.clone()
                    };
                    json!({ "sha256": sha, "size": 1024 })
                }
                "fs.pe_info" => json!({
                    "ok": true, "desc": "x64", "machine": self.machine,
                    "subsystem": "console", "bits": "64",
                }),
                "fs.copy" => {
                    if self.copy_fails.iter().any(|p| p == &from || p == &to) {
                        Fake::err("目标被占用")
                    } else {
                        json!({ "ok": true, "to": to })
                    }
                }
                "proc.status" => {
                    // 真实宿主按 handle 认进程；handle 不是本插件起的就回报“不认识”。
                    let handle = s(params.get("handle"));
                    if !self.handles.iter().any(|h| h == &handle) {
                        json!({ "running": false, "exit_code": -1, "known": false })
                    } else {
                        // 本次刚起的进程是活的，跟「升级前本来就在跑」无关。
                        let live = self.running || self.started_handle.as_deref() == Some(&handle);
                        json!({ "running": live, "pid": 4242 })
                    }
                }
                "proc.stop" => {
                    let handle = s(params.get("handle"));
                    if !self.handles.iter().any(|h| h == &handle) {
                        Fake::err("不认识这个进程句柄")
                    } else {
                        // 第几次调用（calls 里已含本次）：用来让后面的那次停失败
                        let nth = self.calls.iter().filter(|c| *c == "proc.stop").count();
                        json!({ "stopped": self.stop_stopped && nth <= self.stop_true_times })
                    }
                }
                "proc.start" => {
                    if self.start_fails {
                        Fake::err("进程启动后立刻退出（退出码 1）")
                    } else if self.start_nohandle {
                        json!({ "handle": "", "pid": 4343 })
                    } else {
                        // 起一次发一个句柄，宿主从此认得它。
                        let n = self.handles.len();
                        let h = format!("teldrive#{}", n + 8);
                        self.handles.push(h.clone());
                        self.started_handle = Some(h.clone());
                        json!({ "handle": h, "pid": 4343 })
                    }
                }
                "proc.run" => json!({
                    "code": if self.health_ok { 0 } else { 1 },
                    "stdout": if self.health_ok { "READY 200 ok" } else { "TIMEOUT" },
                    "stderr": "",
                }),
                "net.port_open" => json!({ "open": self.port_open }),
                other => panic!("unexpected capability in test: {other}"),
            }
        }
    }

    /// 走真实的 dispatch，把 need 一个个喂回去，最后返回收尾那一轮的回包。
    fn drive(f: &mut Fake, action: &str) -> Value {
        let mut provided = Map::new();
        let mut state = Value::Null;
        for _ in 0..24 {
            let params = json!({
                "action": action,
                "form": {},
                "provided": Value::Object(provided),
                "state": state,
                "lang": "zh",
            });
            let out = crate::dispatch(1, &params);
            if let Some(list) = out.get("need").and_then(|v| v.as_array()) {
                f.states
                    .push(out.get("state").cloned().unwrap_or(Value::Null));
                provided = Map::new();
                for item in list.clone() {
                    let cap = s(item.get("cap"));
                    let key = s(item.get("key"));
                    let p = item.get("params").cloned().unwrap_or(Value::Null);
                    f.calls.push(cap.clone());
                    let v = f.answer(&cap, &p);
                    provided.insert(key, v);
                }
                state = out.get("state").cloned().unwrap_or(Value::Null);
                continue;
            }
            return out;
        }
        panic!("状态机 24 轮内没有结束");
    }

    fn body_of(out: &Value) -> Value {
        out.get("result")
            .and_then(|r| r.get("body"))
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn status_of(out: &Value) -> String {
        s(body_of(out).get("status"))
    }

    #[test]
    fn happy_path_replaces_the_exe_and_keeps_the_version() {
        let mut f = Fake::new();
        f.port_open = true;
        f.running = true;
        let out = drive(&mut f, "update");
        let status = status_of(&out);
        assert!(status.contains(NEW_VER), "status was {status}");
        assert_eq!(
            body_of(&out).get("lamp").and_then(|v| v.as_str()),
            Some("ok")
        );
        assert!(
            f.calls.iter().any(|c| c == "proc.stop"),
            "should stop the owned process"
        );
        assert!(
            f.calls.iter().any(|c| c == "proc.start"),
            "should restart it"
        );
        assert!(
            !f.calls.iter().any(|c| c == "taskkill"),
            "never kill by pid"
        );
        // ops 里必须写新版本
        let ops = out
            .get("ops")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(
            format!("{ops:?}").contains(NEW_VER),
            "ops should record the new version"
        );
    }

    #[test]
    fn a_bad_zip_checksum_never_touches_the_exe() {
        let mut f = Fake::new();
        f.port_open = true;
        f.zip_sha = "9999999999999999999999999999999999999999999999999999999999999999".to_string();
        let out = drive(&mut f, "update");
        assert_eq!(
            body_of(&out).get("lamp").and_then(|v| v.as_str()),
            Some("err")
        );
        assert!(f.calls.iter().any(|c| c == "http.download"));
        assert!(
            !f.calls.iter().any(|c| c == "fs.copy"),
            "must not copy anything: {:?}",
            f.calls
        );
        assert!(
            !f.calls.iter().any(|c| c == "proc.stop"),
            "must not stop the service"
        );
        assert!(!f.calls.iter().any(|c| c == "proc.start"));
    }

    #[test]
    fn a_stage_hash_that_differs_from_the_source_aborts_before_stopping() {
        let mut f = Fake::new();
        f.port_open = true;
        f.stage_sha =
            "8888888888888888888888888888888888888888888888888888888888888888".to_string();
        let out = drive(&mut f, "update");
        assert_eq!(
            body_of(&out).get("lamp").and_then(|v| v.as_str()),
            Some("err")
        );
        assert!(
            !f.calls.iter().any(|c| c == "proc.stop"),
            "no stop before the hash matches"
        );
        assert!(
            !f.calls.iter().any(|c| c == "proc.start"),
            "no start before the hash matches"
        );
        assert!(
            !f.calls.iter().any(|c| c == "proc.run"),
            "no health probe before the hash matches"
        );
    }

    #[test]
    fn an_empty_machine_is_a_failure_not_a_skip() {
        let mut f = Fake::new();
        f.machine = String::new();
        let out = drive(&mut f, "update");
        assert_eq!(
            body_of(&out).get("lamp").and_then(|v| v.as_str()),
            Some("err")
        );
        assert!(status_of(&out).contains("PE") || status_of(&out).contains("架构"));
    }

    #[test]
    fn an_open_port_without_our_handle_refuses_to_update() {
        let mut f = Fake::new();
        f.port_open = true;
        f.kv = json!({ "root": "C:\\data\\plugins\\teldrive", "port": 8080, "version": "2.0.0" });
        let out = drive(&mut f, "update");
        assert_eq!(
            body_of(&out).get("lamp").and_then(|v| v.as_str()),
            Some("err")
        );
        assert!(
            !f.calls.iter().any(|c| c == "proc.stop"),
            "never kill an unknown process"
        );
        assert!(!f.calls.iter().any(|c| c == "proc.start"));
    }

    #[test]
    fn a_failed_health_check_restores_the_backup_and_reports_it() {
        let mut f = Fake::new();
        f.port_open = true;
        f.health_ok = false;
        let out = drive(&mut f, "update");
        let body = body_of(&out);
        let lamp = body.get("lamp").and_then(|v| v.as_str());
        assert_eq!(lamp, Some("err"), "status was {}", status_of(&out));
        let copies = f.calls.iter().filter(|c| *c == "fs.copy").count();
        assert!(
            copies >= 2,
            "expected stage copy + backup restore, saw {}",
            copies
        );
        assert!(status_of(&out).contains("回滚") || status_of(&out).contains("恢复"));
    }

    #[test]
    fn a_failed_swap_copy_rolls_back() {
        let mut f = Fake::new();
        f.port_open = true;
        f.copy_fails = vec!["C:\\data\\plugins\\teldrive\\bin\\teldrive.exe".to_string()];
        let out = drive(&mut f, "update");
        assert_eq!(
            body_of(&out).get("lamp").and_then(|v| v.as_str()),
            Some("err")
        );
    }

    #[test]
    fn the_plan_survives_every_step() {
        let mut f = Fake::new();
        f.port_open = true;
        let out = drive(&mut f, "update");
        assert_eq!(
            body_of(&out).get("lamp").and_then(|v| v.as_str()),
            Some("ok")
        );
        // 字段是一步步填出来的（先有 zip 路径，再有哈希，再有暂存路径），
        // 所以断言的是「填上之后不许再丢」，而不是每一步都齐全。
        let (mut seen_zip, mut sha_seen, mut stage_seen) = (0, false, false);
        for st in &f.states {
            let step = s(st.get("step"));
            let p = Plan::of(st.get("plan").unwrap_or(&Value::Null));
            if p.zip_path.is_empty() {
                assert!(
                    !sha_seen && !stage_seen,
                    "plan shrank back to nothing at step {step}"
                );
                continue;
            }
            seen_zip += 1;
            assert_eq!(p.ver, NEW_VER, "version lost at step {step}");
            assert!(!p.zip_path.is_empty(), "zip path lost at step {step}");
            if p.sha.is_empty() {
                assert!(!sha_seen, "zip checksum lost at step {step}");
            } else {
                assert_eq!(p.sha, SUM, "wrong zip checksum at step {step}");
                sha_seen = true;
            }
            if p.stage.is_empty() {
                assert!(!stage_seen, "stage path lost at step {step}");
            } else {
                stage_seen = true;
            }
        }
        assert!(
            seen_zip >= 6,
            "plan should be visible across steps, saw {seen_zip}"
        );
        assert!(
            sha_seen && stage_seen,
            "checksum or stage path never reached later steps"
        );
    }

    #[test]
    fn a_stopped_service_is_left_stopped() {
        let mut f = Fake::new();
        f.port_open = false;
        f.running = false;
        f.kv = json!({ "root": "C:\\data\\plugins\\teldrive", "port": 8080, "version": "2.0.0" });
        let out = drive(&mut f, "update");
        assert_eq!(
            body_of(&out).get("lamp").and_then(|v| v.as_str()),
            Some("ok")
        );
        // 起一次做健康检查，然后停回去
        assert!(f.calls.iter().any(|c| c == "proc.start"));
        let last_stop = f.calls.iter().position(|c| c == "proc.stop");
        assert!(last_stop.is_some(), "must stop again: {:?}", f.calls);
        let ops = format!("{:?}", out.get("ops"));
        assert!(
            ops.contains("null") || ops.contains("Null"),
            "proc should be cleared: {ops}"
        );
    }

    #[test]
    fn check_reports_up_to_date_when_the_release_is_not_newer() {
        let mut f = Fake::new();
        f.kv = json!({ "root": "C:\\data\\plugins\\teldrive", "port": 8080, "version": "9.9.9" });
        let out = drive(&mut f, "check_update");
        assert_eq!(
            body_of(&out).get("lamp").and_then(|v| v.as_str()),
            Some("ok")
        );
        assert!(status_of(&out).contains("9.9.9"));
    }

    #[test]
    fn check_refuses_when_the_checksum_file_has_no_line_for_the_zip() {
        let mut f = Fake::new();
        f.sums = "aa  teldrive-0.0.1-windows-amd64.zip\n".to_string();
        let out = drive(&mut f, "check_update");
        assert_eq!(
            body_of(&out).get("lamp").and_then(|v| v.as_str()),
            Some("warn")
        );
        let enabled = body_of(&out).get("enabled").cloned().unwrap_or(Value::Null);
        assert_eq!(enabled.get("update").and_then(|v| v.as_bool()), Some(false));
    }

    #[test]
    fn checksum_lines_are_parsed_with_and_without_a_star() {
        let a = format!("{SUM}  dist/teldrive.zip\n");
        let b = format!("{SUM} *teldrive.zip\n");
        assert_eq!(checksum_of(&a, "teldrive.zip").as_deref(), Some(SUM));
        assert_eq!(checksum_of(&b, "teldrive.zip").as_deref(), Some(SUM));
        assert_eq!(checksum_of(&a, "other.zip"), None);
    }

    #[test]
    fn version_order_is_numeric() {
        assert!(ver_gt("2.0.14", "2.0.9"));
        assert!(ver_gt("2.1", "2.0.14"));
        assert!(!ver_gt("2.0.14", "2.0.14"));
        assert!(!ver_gt("1.9.9", "2.0.0"));
    }

    #[test]
    fn arch_names_match_what_the_host_reports() {
        assert_eq!(arch_of(&json!({ "arch": "x86_64" })), "amd64");
        assert_eq!(arch_of(&json!({ "arch": "aarch64" })), "arm64");
        assert_eq!(arch_of(&json!({ "arch": "i686" })), "x86");
        assert_eq!(arch_of(&json!({ "arch": "" })), "");
    }

    #[test]
    fn a_release_without_this_arch_is_reported_as_unsupported() {
        let body: Value =
            serde_json::from_str(&release_json("teldrive-9.9.9-windows-amd64.zip", "v9.9.9"))
                .unwrap();
        assert!(parse_release(&body, "amd64").unwrap().is_some());
        assert!(parse_release(&body, "arm64").unwrap().is_none());
    }

    #[test]
    fn a_body_without_a_tag_is_an_error() {
        let body = json!({ "message": "rate limited" });
        assert!(parse_release(&body, "amd64").is_err());
    }

    #[test]
    fn on_update_without_kv_still_asks_for_it_first() {
        let rq = Rq {
            form: Value::Null,
            provided: Value::Null,
            state: Value::Null,
        };
        let out = on_update(1, &rq);
        let need = out
            .get("need")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(need.iter().any(|n| s(n.get("cap")) == "kv.get_all"));
        assert!(need.iter().any(|n| s(n.get("cap")) == "env.info"));
    }

    /// ops 里写回去的进程句柄（`{"proc":{"handle":"teldrive#9"}}`）。
    fn handles_in_ops(out: &Value) -> Vec<String> {
        fn walk(v: &Value, acc: &mut Vec<String>) {
            match v {
                Value::Object(m) => {
                    for (k, val) in m {
                        if k == "handle" {
                            if let Some(h) = val.as_str() {
                                if !h.is_empty() {
                                    acc.push(h.to_string());
                                }
                            }
                        }
                        walk(val, acc);
                    }
                }
                Value::Array(a) => {
                    for x in a {
                        walk(x, acc);
                    }
                }
                _ => {}
            }
        }
        let mut acc = Vec::new();
        if let Some(ops) = out.get("ops") {
            walk(ops, &mut acc);
        }
        acc
    }

    fn copy_calls(f: &Fake) -> usize {
        f.calls.iter().filter(|c| *c == "fs.copy").count()
    }

    #[test]
    fn a_rollback_that_restarted_the_old_version_keeps_its_handle() {
        let mut f = Fake::new();
        f.port_open = true;
        f.health_ok = false;
        let out = drive(&mut f, "update");
        assert_eq!(
            body_of(&out).get("lamp").and_then(|v| v.as_str()),
            Some("err")
        );
        let ops = format!("{:?}", out.get("ops"));
        assert!(
            !ops.contains(NEW_VER),
            "a failed upgrade must not write the new version: {ops}"
        );
        let hs = handles_in_ops(&out);
        assert!(
            !hs.is_empty(),
            "the restarted old process must keep its handle: {ops}"
        );

        // 拿这个句柄再升级一次：插件应该认得端口上的进程，而不是当成外来的。
        f.kv["proc"] = json!({ "handle": hs[0] });
        f.health_ok = true;
        let again = drive(&mut f, "update");
        assert_eq!(
            body_of(&again).get("lamp").and_then(|v| v.as_str()),
            Some("ok"),
            "second run said: {}",
            status_of(&again)
        );
        assert!(status_of(&again).contains(NEW_VER));
    }

    #[test]
    fn a_process_that_will_not_stop_is_reported_as_a_warning() {
        let mut f = Fake::new();
        f.port_open = false;
        f.running = false;
        f.stop_stopped = false;
        f.kv = json!({ "root": "C:\\data\\plugins\\teldrive", "port": 8080, "version": "2.0.0" });
        let out = drive(&mut f, "update");
        assert_eq!(
            body_of(&out).get("lamp").and_then(|v| v.as_str()),
            Some("warn"),
            "status was {}",
            status_of(&out)
        );
        assert!(
            status_of(&out).contains("没能恢复原来的运行状态"),
            "status was {}",
            status_of(&out)
        );
        // 进程还活着，句柄就得留着，否则用户点停止会被拒。
        let ops = format!("{:?}", out.get("ops"));
        assert!(
            !handles_in_ops(&out).is_empty(),
            "handle lost while the process still runs: {ops}"
        );
    }

    #[test]
    fn a_new_process_that_will_not_stop_blocks_the_restore() {
        let mut f = Fake::new();
        f.port_open = true;
        f.health_ok = false;
        // 停原进程成功（1 次），回滚时停新进程失败
        f.stop_true_times = 1;
        let out = drive(&mut f, "update");
        assert_eq!(
            body_of(&out).get("lamp").and_then(|v| v.as_str()),
            Some("err")
        );
        assert!(
            status_of(&out).contains("停不下来"),
            "status was {}",
            status_of(&out)
        );
        assert_eq!(
            copy_calls(&f),
            3,
            "备份、暂存、换新各一次；新进程没停下来，就不能有恢复那一次覆盖"
        );
        assert!(!format!("{:?}", out.get("ops")).contains(NEW_VER));
    }

    #[test]
    fn a_restore_that_does_not_match_the_backup_is_reported() {
        let mut f = Fake::new();
        f.port_open = true;
        f.health_ok = false;
        // 备份算出来是这个哈希，恢复后算出来却是 EXE：说明换回去的不是原来那个。
        f.bak_sha = "3".repeat(64);
        let out = drive(&mut f, "update");
        assert_eq!(
            body_of(&out).get("lamp").and_then(|v| v.as_str()),
            Some("err")
        );
        assert!(
            status_of(&out).contains("备份"),
            "status was {}",
            status_of(&out)
        );
        assert!(!format!("{:?}", out.get("ops")).contains(NEW_VER));
    }

    #[test]
    fn a_new_process_without_a_handle_is_reported_and_not_rolled_back() {
        let mut f = Fake::new();
        f.port_open = true;
        f.start_nohandle = true;
        let out = drive(&mut f, "update");
        assert_eq!(
            body_of(&out).get("lamp").and_then(|v| v.as_str()),
            Some("err")
        );
        assert!(
            status_of(&out).contains("句柄"),
            "status was {}",
            status_of(&out)
        );
        // 备份、解包暂存、换上新 exe：就这三次。再回滚就会起第二份进程抢端口。
        assert_eq!(copy_calls(&f), 3);
        assert!(
            !format!("{:?}", out.get("ops")).contains(NEW_VER),
            "nothing may be written while the live process is unattributed"
        );
        assert!(handles_in_ops(&out).is_empty());
    }
}

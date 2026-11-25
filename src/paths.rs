// 所有会变化的文件都放在一个根目录下，根目录由面板上的「数据目录」决定，
// 留空时用 <RCGUI 数据目录>\plugins\teldrive（env.info.data_dir 给的就是前者）。
//
//   root\
//     bin\teldrive.exe        服务端
//     bin\rclone.exe          带 teldrive 后端的 fork
//     tmp\                    下载与解压的临时区，部署成功后清空
//     scripts\secrets.ps1     生成 signing-key / data-key
//     scripts\pg.ps1          便携版 PostgreSQL 的初始化与建库（不负责启动服务）
//     logs\teldrive.log       proc.start 的 silent=false 日志
//     logs\pg.log             postgres 自己的日志
//     pg\pgsql\...            便携版 PostgreSQL 解压结果
//     pgdata\                 initdb 生成的集群目录
//     config.yaml             teldrive 配置

pub struct Paths {
    pub root: String,
    pub bin: String,
    pub tmp: String,
    pub scripts: String,
    pub logs: String,
    pub pg: String,
    pub pgdata: String,
    pub cfg: String,
    pub log_file: String,
}

// 插件的 wasm 是 wasm32-wasip1（类 Unix），PathBuf 拼出来的分隔符是 `/`，
// 和用户填的 `D:\teldrive` 混在一起会变成 `D:\teldrive/bin/teldrive.exe`。
// Windows 能吃这种混合路径，但面板上显示、日志里读都别扭，所以统一换成 `\`。
fn join(base: &str, parts: &[&str]) -> String {
    let mut p = std::path::PathBuf::from(base);
    for part in parts {
        p.push(part);
    }
    p.to_string_lossy().replace('/', "\\")
}

impl Paths {
    pub fn new(root: &str) -> Self {
        let root = root.trim_end_matches(['\\', '/']).to_string();
        Paths {
            bin: join(&root, &["bin"]),
            tmp: join(&root, &["tmp"]),
            scripts: join(&root, &["scripts"]),
            logs: join(&root, &["logs"]),
            pg: join(&root, &["pg"]),
            pgdata: join(&root, &["pgdata"]),
            cfg: join(&root, &["config.yaml"]),
            log_file: join(&root, &["logs", "teldrive.log"]),
            root,
        }
    }

    /// root 为空时回落到 RCGUI 数据目录里的固定子目录。
    pub fn resolve(data_dir: &str, form_root: &str) -> Self {
        let root = if form_root.trim().is_empty() {
            join(data_dir, &["plugins", "teldrive"])
        } else {
            form_root.trim().to_string()
        };
        Paths::new(&root)
    }

    pub fn teldrive_exe(&self) -> String {
        join(&self.bin, &["teldrive.exe"])
    }

    pub fn rclone_exe(&self) -> String {
        join(&self.bin, &["rclone.exe"])
    }

    pub fn secrets_script(&self) -> String {
        join(&self.scripts, &["secrets.ps1"])
    }

    pub fn pg_script(&self) -> String {
        join(&self.scripts, &["pg.ps1"])
    }

    /// 便携版 PostgreSQL 的 bin 目录：tar 把 txz 里的 pgsql\ 解到 `pg` 下，所以是 pg\pgsql\bin。
    pub fn pg_bin(&self) -> String {
        join(&self.pg, &["pgsql", "bin"])
    }

    /// 数据库服务本身。插件用 `proc.start` 拉起它，所以要有单独的路径。
    pub fn pg_exe(&self) -> String {
        join(&self.pg_bin(), &["postgres.exe"])
    }

    /// 数据库服务的输出。postgres 把它当 stdout/stderr 追加，起来失败的原因都在里面。
    pub fn pg_log(&self) -> String {
        join(&self.logs, &["pg.log"])
    }

    /// 扫码登录用的二维码图片。面板的 `kind:"image"` 控件按这个路径读盘显示，
    /// 所以必须落在磁盘上；每次扫码都覆盖同一个文件，宿主的图片缓存不会跟着换。
    pub fn qr_png(&self) -> String {
        join(&self.root, &["login-qr.png"])
    }
}

//! 二维码编码与 PNG 输出，只为把 teldrive 给的 `tg://login?token=…` 显示成一张能扫的图。
//!
//! 面板里有 `kind:"image"` 控件：插件把 PNG 写到本地，再把绝对路径放进
//! `body.images.<控件 key>`，宿主读盘解码后显示。所以这里只负责「文本 → PNG 字节」，
//! 落盘由 `store` 走 `fs.write_b64`。宿主只链了 `image` crate 的 png 特性，没有 SVG
//! 支持，因此不能输出 SVG。
//!
//! 实现范围是够用就好：字节模式 + 纠错等级 M + 版本 1-20。
//!
//! Telegram 的登录链接是 `tg://login?token=` 加 base64url 的 token：前缀 19 字节，
//! token 本身 200 字节级，整条往往 250-300 字节。v10（M 级 216 个数据码字）装不下
//! 这种长链接，所以版本上限放到 20（M 级 669 个数据码字，约 660 字节），留足余量。
//!
//! 文本连版本 20 都装不下时 `png` 返回 `None`，调用方应提示改用手机号登录。

/// 纠错等级 M 在格式信息里的两位编码是 `00`。
const ECL_M: u32 = 0b00;

struct Spec {
    /// 整份码字总数（数据 + 纠错），决定码图尺寸与容量校验。
    total: usize,
    /// 数据码字总数。
    data: usize,
    /// 每块的纠错码字数。
    ecc: usize,
    /// 分块数。
    blocks: usize,
}

/// 版本 1-20 在纠错等级 M 下的标准参数表。
///
/// 数值取自 python-qrcode 的 `RS_BLOCK_TABLE`（M 行），与 ISO/IEC 18004 表 9 一致。
const SPECS: [Spec; 20] = [
    Spec { total: 26, data: 16, ecc: 10, blocks: 1 },
    Spec { total: 44, data: 28, ecc: 16, blocks: 1 },
    Spec { total: 70, data: 44, ecc: 26, blocks: 1 },
    Spec { total: 100, data: 64, ecc: 18, blocks: 2 },
    Spec { total: 134, data: 86, ecc: 24, blocks: 2 },
    Spec { total: 172, data: 108, ecc: 16, blocks: 4 },
    Spec { total: 196, data: 124, ecc: 18, blocks: 4 },
    Spec { total: 242, data: 154, ecc: 22, blocks: 4 },
    Spec { total: 292, data: 182, ecc: 22, blocks: 5 },
    Spec { total: 346, data: 216, ecc: 26, blocks: 5 },
    Spec { total: 404, data: 254, ecc: 30, blocks: 5 },
    Spec { total: 466, data: 290, ecc: 22, blocks: 8 },
    Spec { total: 532, data: 334, ecc: 22, blocks: 9 },
    Spec { total: 581, data: 365, ecc: 24, blocks: 9 },
    Spec { total: 655, data: 415, ecc: 24, blocks: 10 },
    Spec { total: 733, data: 453, ecc: 28, blocks: 10 },
    Spec { total: 815, data: 507, ecc: 28, blocks: 11 },
    Spec { total: 901, data: 563, ecc: 26, blocks: 13 },
    Spec { total: 991, data: 627, ecc: 26, blocks: 14 },
    Spec { total: 1085, data: 669, ecc: 26, blocks: 16 },
];

/// 每个版本的对齐图形中心坐标（版本 1 没有对齐图形）。
const ALIGN: [&[usize]; 20] = [
    &[],
    &[6, 18],
    &[6, 22],
    &[6, 26],
    &[6, 30],
    &[6, 34],
    &[6, 22, 38],
    &[6, 24, 42],
    &[6, 26, 46],
    &[6, 28, 50],
    &[6, 30, 54],
    &[6, 32, 58],
    &[6, 34, 62],
    &[6, 26, 46, 66],
    &[6, 26, 48, 70],
    &[6, 26, 50, 74],
    &[6, 30, 54, 78],
    &[6, 30, 56, 82],
    &[6, 30, 58, 86],
    &[6, 34, 62, 90],
];

/// 静区宽度（模块数）。扫描器靠这段空白定位，少于 4 个模块常常扫不出来。
const QUIET: usize = 4;
/// 每个模块占几个像元。面板会把图缩到约 210 px 高，再大也不加信息量；
/// 4 已经能在二维码最宽处（版本 20 = 97 模块）压到 420 px。
const SCALE: usize = 4;

/// 把 `text` 编成二维码并返回 PNG 字节；文本超过版本 20 的容量时返回 `None`。
///
/// 输出是 1 位灰度的 PNG：比 8 位灰度小 8 倍（v11 的码约 15 KB），而二维码只有黑白
/// 两色，不需要灰阶。每个模块 `SCALE` 像元、四周 `QUIET` 个模块的静区。
pub fn png(text: &str) -> Option<Vec<u8>> {
    let modules = encode(text)?;
    let side = (modules.len() + QUIET * 2) * SCALE;
    let raw = scanlines(&modules, side);

    let mut out = Vec::with_capacity(raw.len() / 4 + 128);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);

    // IHDR：宽、高、位深 1、颜色类型 0（灰度）、压缩 0、过滤 0、不隔行。
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(side as u32).to_be_bytes());
    ihdr.extend_from_slice(&(side as u32).to_be_bytes());
    ihdr.extend_from_slice(&[1, 0, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", &[]);
    Some(out)
}

/// 标准 base64（带 `=` 补齐）。宿主 `fs.write_b64` 只收 base64 文本。
pub fn b64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for group in data.chunks(3) {
        let b1 = group.get(1).copied().unwrap_or(0);
        let b2 = group.get(2).copied().unwrap_or(0);
        let word = ((group[0] as u32) << 16) | ((b1 as u32) << 8) | b2 as u32;
        out.push(TABLE[(word >> 18) as usize & 63] as char);
        out.push(TABLE[(word >> 12) as usize & 63] as char);
        out.push(if group.len() > 1 {
            TABLE[(word >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if group.len() > 2 {
            TABLE[word as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// 逐行拼扫描线：每行先是 filter 字节 0（不做行预测），再是打包好的 1 位像素。
///
/// PNG 的 1 位灰度里位值 1 表示白、0 表示黑，所以每行先填满 0xFF 再把深色模块的
/// 位清掉；静区落在码图之外，自动保持白色。
fn scanlines(modules: &[Vec<bool>], side: usize) -> Vec<u8> {
    let row_len = side.div_ceil(8);
    let mut raw = Vec::with_capacity((row_len + 1) * side);
    let mut row = vec![0xFFu8; row_len + 1];
    for y in 0..side {
        row.fill(0xFF);
        row[0] = 0;
        let my = y / SCALE;
        if my >= QUIET && my < QUIET + modules.len() {
            let line = &modules[my - QUIET];
            for (mx, dark) in line.iter().enumerate() {
                if !*dark {
                    continue;
                }
                let x0 = (mx + QUIET) * SCALE;
                for x in x0..x0 + SCALE {
                    row[1 + x / 8] &= !(0x80u8 >> (x % 8));
                }
            }
        }
        raw.extend_from_slice(&row);
    }
    raw
}

/// 写一个 PNG chunk：长度、类型、数据、CRC32（CRC 覆盖类型和数据）。
fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// 包一层 zlib：2 字节头 + 若干「不压缩」deflate 块 + Adler-32。
///
/// 用 stored 块而不是真压缩，是因为这里的数据是二维码（高频条纹），
/// 自己写 deflate 压缩器不划算，而 stored 块只多 5 字节/64 KB。
fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + raw.len() / 65_535 * 5 + 16);
    out.extend_from_slice(&[0x78, 0x01]);
    if raw.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);
    } else {
        let mut at = 0usize;
        while at < raw.len() {
            let take = (raw.len() - at).min(65_535);
            let last = at + take == raw.len();
            out.push(u8::from(last));
            let len = take as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(&raw[at..at + take]);
            at += take;
        }
    }
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    let (mut low, mut high) = (1u32, 0u32);
    for byte in data {
        low = (low + *byte as u32) % 65_521;
        high = (high + low) % 65_521;
    }
    (high << 16) | low
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// 选最小的够用版本，然后编码、挑掩码、返回模块矩阵（`true` = 深色）。
fn encode(text: &str) -> Option<Vec<Vec<bool>>> {
    let bytes = text.as_bytes();
    let version = pick_version(bytes.len())?;
    let spec = &SPECS[version - 1];
    let data = pad_to(&data_bits(bytes, version, spec), spec.data);
    let codewords = interleave(&data, spec);

    // 八种掩码都生成一遍，取罚分最低的（规范要求，也影响扫码成功率）。
    let mut best: Option<(i32, Vec<Vec<bool>>)> = None;
    for mask in 0..8u8 {
        let grid = build(&codewords, version, mask);
        let score = penalty(&grid);
        if best.as_ref().map(|(s, _)| score < *s).unwrap_or(true) {
            best = Some((score, grid));
        }
    }
    best.map(|(_, grid)| grid)
}

/// 找能装下 `len` 字节的最小版本；都不够就 `None`。
fn pick_version(len: usize) -> Option<usize> {
    for v in 1..=SPECS.len() {
        let spec = &SPECS[v - 1];
        let count_bits = if v <= 9 { 8 } else { 16 };
        if 4 + count_bits + len * 8 <= spec.data * 8 {
            return Some(v);
        }
    }
    None
}

/// 字节模式的比特流：模式指示符 `0100` + 长度 + 数据。
///
/// 长度字段的位数随版本变化（v1-9 是 8 位，v10 及以后是 16 位），这是规范要求，
/// 写错会让扫码器按错误长度解读。
fn data_bits(bytes: &[u8], version: usize, _spec: &Spec) -> Vec<bool> {
    let mut bits = Vec::with_capacity(bytes.len() * 8 + 20);
    push_bits(&mut bits, 0b0100, 4);
    push_bits(&mut bits, bytes.len() as u32, if version <= 9 { 8 } else { 16 });
    for b in bytes {
        push_bits(&mut bits, *b as u32, 8);
    }
    bits
}

fn push_bits(bits: &mut Vec<bool>, value: u32, width: usize) {
    for i in (0..width).rev() {
        bits.push((value >> i) & 1 == 1);
    }
}

/// 补终止符、补齐到字节边界、再交替填 `0xEC`/`0x11` 直到数据码字填满。
fn pad_to(bits: &[bool], data_codewords: usize) -> Vec<u8> {
    let mut bits = bits.to_vec();
    let cap = data_codewords * 8;
    for _ in 0..4 {
        if bits.len() < cap {
            bits.push(false);
        }
    }
    while bits.len() % 8 != 0 {
        bits.push(false);
    }
    let mut out = Vec::with_capacity(data_codewords);
    for chunk in bits.chunks(8) {
        let mut byte = 0u8;
        for (i, bit) in chunk.iter().enumerate() {
            if *bit {
                byte |= 1 << (7 - i);
            }
        }
        out.push(byte);
    }
    let mut pads = [0xECu8, 0x11u8].iter().cycle();
    while out.len() < data_codewords {
        out.push(*pads.next().unwrap());
    }
    out
}

/// 按规范分块、算纠错码字、再交错排成最终码字序列。
///
/// 块长用「整除 + 余数」推：短块在前、长块在后（v8 的 154 个数据码字分成
/// 2 块 38 加 2 块 39，就是这么来的）。
fn interleave(data: &[u8], spec: &Spec) -> Vec<u8> {
    let n = spec.blocks;
    let short = spec.data / n;
    let long_count = spec.data % n;

    let mut blocks: Vec<Vec<u8>> = Vec::with_capacity(n);
    let mut pos = 0usize;
    for i in 0..n {
        let len = if i < n - long_count { short } else { short + 1 };
        blocks.push(data[pos..pos + len].to_vec());
        pos += len;
    }

    let gen = rs_generator(spec.ecc);
    let eccs: Vec<Vec<u8>> = blocks.iter().map(|b| rs_remainder(b, &gen)).collect();

    let mut out = Vec::with_capacity(spec.total);
    let longest = blocks.iter().map(|b| b.len()).max().unwrap_or(0);
    for col in 0..longest {
        for b in &blocks {
            if col < b.len() {
                out.push(b[col]);
            }
        }
    }
    for col in 0..spec.ecc {
        for e in &eccs {
            out.push(e[col]);
        }
    }
    out
}

fn gf_mul(a: u8, b: u8) -> u8 {
    let mut z = 0u32;
    let mut x = a as u32;
    let mut y = b as u32;
    for _ in 0..8 {
        if y & 1 != 0 {
            z ^= x;
        }
        y >>= 1;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= 0x11D; // QR 用的本原多项式 x^8+x^4+x^3+x^2+1
        }
    }
    z as u8
}

fn gf_pow2(n: u32) -> u8 {
    let mut r = 1u8;
    for _ in 0..n {
        r = gf_mul(r, 2);
    }
    r
}

/// 生成多项式 g(x)=∏(x-α^i)，系数按最高次在前排列。
fn rs_generator(degree: usize) -> Vec<u8> {
    let mut g = vec![1u8];
    for i in 0..degree {
        let root = gf_pow2(i as u32);
        let mut next = vec![0u8; g.len() + 1];
        for (j, c) in g.iter().enumerate() {
            next[j] ^= *c;
            next[j + 1] ^= gf_mul(*c, root);
        }
        g = next;
    }
    g
}

/// 多项式除法取余，余数就是该块的纠错码字。
fn rs_remainder(data: &[u8], gen: &[u8]) -> Vec<u8> {
    let ecc = gen.len() - 1;
    let mut buf = vec![0u8; data.len() + ecc];
    buf[..data.len()].copy_from_slice(data);
    for i in 0..data.len() {
        let factor = buf[i];
        if factor == 0 {
            continue;
        }
        for (j, g) in gen.iter().enumerate() {
            buf[i + j] ^= gf_mul(*g, factor);
        }
    }
    buf[data.len()..].to_vec()
}

/// 画功能图形（定位、时序、对齐、格式信息、版本信息），再按掩码铺数据。
fn build(codewords: &[u8], version: usize, mask: u8) -> Vec<Vec<bool>> {
    let size = 17 + 4 * version;
    let mut m: Vec<Vec<Option<bool>>> = vec![vec![None; size]; size];

    for &(r, c) in &[(0usize, 0usize), (0, size - 7), (size - 7, 0)] {
        place_finder(&mut m, r, c);
    }
    for i in 8..size - 8 {
        let dark = i % 2 == 0;
        m[6][i] = Some(dark);
        m[i][6] = Some(dark);
    }
    place_alignment(&mut m, version);
    place_format(&mut m, mask);
    if version >= 7 {
        place_version(&mut m, version);
    }
    m[size - 8][8] = Some(true); // 规范固定要求的深色模块
    map_data(&mut m, codewords, mask);

    m.into_iter()
        .map(|row| row.into_iter().map(|v| v.unwrap_or(false)).collect())
        .collect()
}

fn place_finder(m: &mut [Vec<Option<bool>>], row: usize, col: usize) {
    let size = m.len();
    for dr in -1i32..=7 {
        for dc in -1i32..=7 {
            let r = row as i32 + dr;
            let c = col as i32 + dc;
            if r < 0 || c < 0 || r >= size as i32 || c >= size as i32 {
                continue;
            }
            let dark = (0..=6).contains(&dr)
                && (0..=6).contains(&dc)
                && (dr == 0 || dr == 6 || dc == 0 || dc == 6 || ((2..=4).contains(&dr) && (2..=4).contains(&dc)));
            m[r as usize][c as usize] = Some(dark);
        }
    }
}

fn place_alignment(m: &mut [Vec<Option<bool>>], version: usize) {
    let pos = ALIGN[version - 1];
    let last = pos.len().saturating_sub(1);
    for (i, &row) in pos.iter().enumerate() {
        for (j, &col) in pos.iter().enumerate() {
            // 与三个定位图形重叠的位置不放对齐图形
            if (i == 0 && j == 0) || (i == 0 && j == last) || (i == last && j == 0) {
                continue;
            }
            for dr in -2i32..=2 {
                for dc in -2i32..=2 {
                    let dark = dr.abs() == 2 || dc.abs() == 2 || (dr == 0 && dc == 0);
                    m[(row as i32 + dr) as usize][(col as i32 + dc) as usize] = Some(dark);
                }
            }
        }
    }
}

/// 格式信息的 15 位：5 位数据 + 10 位 BCH 余数，再异或 `0x5412`。
fn place_format(m: &mut [Vec<Option<bool>>], mask: u8) {
    let size = m.len();
    let data = (ECL_M << 3) | mask as u32;
    let bits = ((data << 10) | bch_remainder(data << 10, 0x537, 10)) ^ 0x5412;
    for i in 0..15usize {
        let dark = (bits >> i) & 1 == 1;
        let (r1, c1) = if i < 6 {
            (i, 8)
        } else if i < 8 {
            (i + 1, 8)
        } else {
            (size - 15 + i, 8)
        };
        m[r1][c1] = Some(dark);

        let (r2, c2) = if i < 8 {
            (8, size - 1 - i)
        } else if i == 8 {
            (8, 7)
        } else {
            (8, 14 - i)
        };
        m[r2][c2] = Some(dark);
    }
}

/// 版本 7 及以上才有版本信息：6 位版本号 + 12 位 BCH 余数。
fn place_version(m: &mut [Vec<Option<bool>>], version: usize) {
    let size = m.len();
    let data = version as u32;
    let bits = (data << 12) | bch_remainder(data << 12, 0x1F25, 12);
    for i in 0..18usize {
        let dark = (bits >> i) & 1 == 1;
        let r = i / 3;
        let c = i % 3 + size - 11;
        m[r][c] = Some(dark);
        m[c][r] = Some(dark);
    }
}

/// 从右下角起、两列一组蛇形填数据位；`None` 的格子才算数据区。
fn map_data(m: &mut [Vec<Option<bool>>], codewords: &[u8], mask: u8) {
    let size = m.len();
    let mut bit_index = 7i32;
    let mut byte_index = 0usize;
    let mut row = size as i32 - 1;
    let mut inc = -1i32;

    let mut col = size as i32 - 1;
    while col > 0 {
        if col == 6 {
            col -= 1; // 第 6 列是时序图形，跳过
        }
        loop {
            for c in 0..2 {
                let cc = (col - c) as usize;
                let rr = row as usize;
                if m[rr][cc].is_some() {
                    continue;
                }
                let mut dark = false;
                if byte_index < codewords.len() {
                    dark = (codewords[byte_index] >> bit_index) & 1 == 1;
                }
                if mask_bit(mask, rr, cc) {
                    dark = !dark;
                }
                m[rr][cc] = Some(dark);
                bit_index -= 1;
                if bit_index < 0 {
                    bit_index = 7;
                    byte_index += 1;
                }
            }
            row += inc;
            if row < 0 || row >= size as i32 {
                row -= inc;
                inc = -inc;
                break;
            }
        }
        col -= 2;
    }
}

fn mask_bit(mask: u8, r: usize, c: usize) -> bool {
    match mask {
        0 => (r + c) % 2 == 0,
        1 => r % 2 == 0,
        2 => c % 3 == 0,
        3 => (r + c) % 3 == 0,
        4 => (r / 2 + c / 3) % 2 == 0,
        5 => (r * c) % 2 + (r * c) % 3 == 0,
        6 => ((r * c) % 2 + (r * c) % 3) % 2 == 0,
        7 => ((r + c) % 2 + (r * c) % 3) % 2 == 0,
        _ => false,
    }
}

/// 多项式除法的余数（GF(2)，`poly` 的阶为 `degree`）。
fn bch_remainder(value: u32, poly: u32, degree: u32) -> u32 {
    let mut v = value;
    while v != 0 {
        let top = 31 - v.leading_zeros();
        if top < degree {
            break;
        }
        v ^= poly << (top - degree);
    }
    v
}

/// 规范里的四条罚分规则，用来挑掩码；分数越低越好扫。
fn penalty(grid: &[Vec<bool>]) -> i32 {
    let size = grid.len();
    let mut score = 0i32;

    // 规则 1：行/列里连续 5 个以上同色
    for i in 0..size {
        for horizontal in [true, false] {
            let mut run = 1i32;
            for j in 1..size {
                let prev = if horizontal { grid[i][j - 1] } else { grid[j - 1][i] };
                let cur = if horizontal { grid[i][j] } else { grid[j][i] };
                if cur == prev {
                    run += 1;
                } else {
                    if run >= 5 {
                        score += 3 + (run - 5);
                    }
                    run = 1;
                }
            }
            if run >= 5 {
                score += 3 + (run - 5);
            }
        }
    }

    // 规则 2：2x2 同色方块
    for r in 0..size - 1 {
        for c in 0..size - 1 {
            let v = grid[r][c];
            if v == grid[r][c + 1] && v == grid[r + 1][c] && v == grid[r + 1][c + 1] {
                score += 3;
            }
        }
    }

    // 规则 3：形如 1:1:3:1:1 的定位形状（找图案时会误判）
    let pat = [true, false, true, true, true, false, true];
    for i in 0..size {
        for j in 0..size.saturating_sub(6) {
            let ok_row = (0..7).all(|k| grid[i][j + k] == pat[k]);
            let ok_col = (0..7).all(|k| grid[j + k][i] == pat[k]);
            if ok_row && ((j + 7..j + 11).all(|c| c >= size || !grid[i][c]) || (0..4).all(|k| j < k || !grid[i][j - k]))
            {
                score += 40;
            }
            if ok_col && ((j + 7..j + 11).all(|r| r >= size || !grid[r][i]) || (0..4).all(|k| j < k || !grid[j - k][i]))
            {
                score += 40;
            }
        }
    }

    // 规则 4：深色模块比例偏离 50%
    let dark = grid.iter().flatten().filter(|v| **v).count() as i32;
    let total = (size * size) as i32;
    let percent = dark * 100 / total;
    let deviation = (percent - 50).abs() / 5;
    score += deviation * 10;

    score
}

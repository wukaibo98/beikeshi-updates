//! 管理页登录：口令走环境变量，登录后发一个签名 Cookie。
//!
//! 不引 JWT 也不引 session 表：就一台机器、一个管理员，
//! "HMAC(随机串) 塞进 HttpOnly Cookie" 已经够了，而且没有服务端状态。

use std::fs;
use std::path::Path;

use hmac::{Hmac, Mac};
use rand::distributions::Alphanumeric;
use rand::Rng;
use sha2::Sha256;

pub const COOKIE: &str = "bks_admin";
const MAX_AGE: u32 = 86400 * 7;

pub struct Auth {
    secret: Vec<u8>,
}

impl Auth {
    /// 密钥存在数据目录里，第一次启动随机生成。丢了只是所有会话失效。
    pub fn load_or_create(path: &Path) -> std::io::Result<Self> {
        let secret = if path.exists() {
            fs::read(path)?
        } else {
            let bytes: Vec<u8> = rand::thread_rng().sample_iter(&Alphanumeric).take(48).collect();
            fs::write(path, &bytes)?;
            bytes
        };
        Ok(Self { secret })
    }

    fn sign(&self, value: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret)
            .expect("HMAC 接受任意长度密钥");
        mac.update(value.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// 从 Cookie 头里挑出我们的那一条并校验签名。
    ///
    /// 返回管理端人员的 id：**0 = 环境变量口令（超级管理员）**，其他是 `admin_users` 表里的 id。
    pub fn verify(&self, cookie_header: Option<&str>) -> Option<i64> {
        let value = extract_cookie(cookie_header, COOKIE)?;
        // 格式：a<id>.<nonce>.<sig>
        let mut parts = value.splitn(3, '.');
        let id_part = parts.next()?;
        let nonce = parts.next()?;
        let sig = parts.next()?;
        if nonce.is_empty() || sig.is_empty() {
            return None;
        }
        let payload = format!("{id_part}.{nonce}");
        if !constant_time_eq(sig.as_bytes(), self.sign(&payload).as_bytes()) {
            return None;
        }
        // Cookie 里的 id 只是给人看的；真正的权限靠登录时校验过。
        id_part.strip_prefix('a')?.parse().ok()
    }

    /// 发登录 Cookie。`admin_id` = 0 表示环境变量口令（超级管理员）。
    pub fn issue_cookie(&self, admin_id: i64) -> String {
        let nonce: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(32)
            .map(char::from)
            .collect();
        let payload = format!("a{admin_id}.{nonce}");
        let value = format!("{payload}.{}", self.sign(&payload));
        format!("{COOKIE}={value}; HttpOnly; SameSite=Lax; Path=/; Max-Age={MAX_AGE}")
    }

    pub fn clear_cookie(&self) -> String {
        format!("{COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0")
    }

    /* ------------------------------------------------ 表单用 token */

    /// 表单登录后的用户 token：`u<user_id>.<exp>.<sig>`。
    /// 客户端把它放在 `X-Bks-Form` 请求头里（不用 Cookie：客户端是原生 HTTP，没有浏览器会话）。
    /// 无状态，服务端重启不影响已登录的人。
    pub fn issue_form_token(&self, user_id: i64, ttl_secs: i64) -> (String, i64) {
        let exp = crate::db::now() + ttl_secs;
        let payload = format!("u{user_id}.{exp}");
        let token = format!("{payload}.{}", self.sign(&payload));
        (token, exp)
    }

    /// 校验通过返回 user_id。
    pub fn verify_form_token(&self, token: &str) -> Option<i64> {
        let (payload, sig) = token.rsplit_once('.')?;
        if !constant_time_eq(sig.as_bytes(), self.sign(payload).as_bytes()) {
            return None;
        }
        let rest = payload.strip_prefix('u')?;
        let (id, exp) = rest.split_once('.')?;
        if exp.parse::<i64>().ok()? < crate::db::now() {
            return None;
        }
        id.parse().ok()
    }

    /// 编辑模板用的 token（管理员输一次口令换来的）：`e.<exp>.<sig>`。
    pub fn issue_edit_token(&self, ttl_secs: i64) -> (String, i64) {
        let exp = crate::db::now() + ttl_secs;
        let payload = format!("e.{exp}");
        let token = format!("{payload}.{}", self.sign(&payload));
        (token, exp)
    }

    pub fn verify_edit_token(&self, token: &str) -> bool {
        let Some((payload, sig)) = token.rsplit_once('.') else { return false };
        if !constant_time_eq(sig.as_bytes(), self.sign(payload).as_bytes()) {
            return false;
        }
        let Some(rest) = payload.strip_prefix('e') else { return false };
        let Some(exp) = rest.strip_prefix('.') else { return false };
        exp.parse::<i64>().map(|e| e >= crate::db::now()).unwrap_or(false)
    }
}

fn extract_cookie(header: Option<&str>, name: &str) -> Option<String> {
    let header = header?;
    for part in header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(&format!("{name}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

/// 逐字节比较，不因为提前返回而泄露"前几位对了"。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

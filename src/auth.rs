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
    pub fn verify(&self, cookie_header: Option<&str>) -> bool {
        let Some(value) = extract_cookie(cookie_header, COOKIE) else {
            return false;
        };
        let Some((nonce, sig)) = value.split_once('.') else {
            return false;
        };
        if nonce.is_empty() || sig.is_empty() {
            return false;
        }
        constant_time_eq(sig.as_bytes(), self.sign(nonce).as_bytes())
    }

    pub fn issue_cookie(&self) -> String {
        let nonce: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(32)
            .map(char::from)
            .collect();
        let value = format!("{nonce}.{}", self.sign(&nonce));
        format!("{COOKIE}={value}; HttpOnly; SameSite=Lax; Path=/; Max-Age={MAX_AGE}")
    }

    pub fn clear_cookie(&self) -> String {
        format!("{COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0")
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

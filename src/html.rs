//! 管理页 HTML。
//!
//! 用 `%%KEY%%` 占位再替换，而不是 `format!` —— 页面里有大量 CSS 花括号，
//! `format!` 会把它们当占位符，写起来满屏 `{{`。原 Python 版也是这么干的。

pub const LOGIN: &str = r##"<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>登录 · 贝可施更新分发</title>
<style>
*{box-sizing:border-box}
body{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;
  background:#F4F7EE;color:#232B1C;font:14px/1.6 "Microsoft YaHei UI","PingFang SC",system-ui,sans-serif}
.wrap{width:320px;background:#fff;border:1px solid #E2E8D8;border-radius:14px;padding:26px 24px;
  box-shadow:0 6px 24px rgba(35,43,28,.06)}
h1{margin:0 0 4px;font-size:17px;color:#24400D}
p.sub{margin:0 0 18px;font-size:12px;color:#66705C}
input{width:100%;height:38px;border:1px solid #D8E0CC;border-radius:9px;background:#F8FAF3;
  padding:0 12px;font:inherit;outline:none}
input:focus{border-color:#3E6B1B;background:#fff;box-shadow:0 0 0 3px rgba(62,107,27,.12)}
button{width:100%;height:38px;margin-top:12px;border:0;border-radius:9px;background:#3E6B1B;
  color:#fff;font:inherit;font-size:14px;cursor:pointer}
button:hover{background:#335915}
.err{margin-top:12px;color:#A32D2D;font-size:12px}
</style></head>
<body><div class="wrap">
  <h1>贝可施更新分发</h1>
  <p class="sub">请输入管理口令</p>
  <form method="post" action="/admin/login">
    <input type="password" name="token" autofocus required placeholder="管理口令">
    <button type="submit">登录</button>
  </form>
  %%ERROR%%
</div></body></html>"##;

pub const ADMIN: &str = r##"<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>贝可施更新分发</title>
<style>
*{box-sizing:border-box}
body{margin:0;background:#F4F7EE;color:#232B1C;
  font:14px/1.6 "Microsoft YaHei UI","PingFang SC",system-ui,sans-serif}
.wrap{max-width:820px;margin:0 auto;padding:28px 20px 60px}
h1{font-size:19px;color:#24400D;margin:0 0 2px}
.sub{font-size:12px;color:#66705C;margin-bottom:18px}
.card{background:#fff;border:1px solid #E2E8D8;border-radius:14px;padding:18px 20px;margin-bottom:16px}
.card h2{font-size:13px;margin:0 0 14px;color:#24400D;font-weight:500}
label{display:block;font-size:12px;color:#66705C;margin:10px 0 4px}
input[type=text],input[type=password],select,textarea{width:100%;border:1px solid #D8E0CC;
  border-radius:8px;background:#F8FAF3;padding:8px 10px;font:inherit;font-size:13px;outline:none}
input[type=file]{font-size:12px}
textarea{resize:vertical;min-height:56px}
button{font:inherit;font-size:13px;height:34px;padding:0 16px;border:0;border-radius:8px;
  background:#3E6B1B;color:#fff;cursor:pointer}
button:hover{background:#335915}
button.ghost{background:transparent;border:1px solid #D8E0CC;color:#66705C;height:28px;padding:0 10px;font-size:12px}
button.ghost:hover{border-color:#3E6B1B;color:#3E6B1B}
button.danger{background:transparent;border:1px solid #F0C0BC;color:#A32D2D;height:28px;padding:0 10px;font-size:12px}
button.danger:hover{background:#FDF4F3}
table{width:100%;border-collapse:collapse;font-size:13px}
th{font-weight:500;color:#66705C;font-size:12px;text-align:left;padding:6px 8px;border-bottom:1px solid #EFF3E7}
td{padding:8px;border-bottom:1px solid #EFF3E7;vertical-align:top}
code{background:#F1F5E9;padding:1px 5px;border-radius:4px;font-size:12px}
.tag{display:inline-block;font-size:11px;padding:1px 7px;border-radius:20px;background:#E7EDDC;color:#3E6B1B}
.tag.py{background:#EEF1F6;color:#3A5A8C}
.muted{color:#98A08F;font-size:12px}
.flash{background:#EAF3DE;color:#27500A;padding:8px 12px;border-radius:10px;font-size:13px;margin-bottom:14px}
.err{background:#FCEBEB;color:#791F1F;padding:8px 12px;border-radius:10px;font-size:13px;margin-bottom:14px}
.row{display:flex;gap:6px;flex-wrap:wrap}
.top{display:flex;justify-content:space-between;align-items:flex-start;gap:12px}
</style></head>
<body><div class="wrap">
  <div class="top">
    <div>
      <h1>贝可施更新分发</h1>
      <div class="sub">%%SUBTITLE%%</div>
    </div>
    <form method="post" action="/admin/logout"><button class="ghost" type="submit">退出</button></form>
  </div>
  %%FLASH%%
  <div class="card">
    <h2>发布新版本</h2>
    <form method="post" action="/admin/upload" enctype="multipart/form-data">
      <label>版本号（形如 1.2.0）</label>
      <input type="text" name="version" placeholder="1.2.0" required>
      <label>更新说明（会显示在客户端，一句话）</label>
      <textarea name="notes" placeholder="修复赠品折合"></textarea>
      <label>程序文件（exe，上传后自动算 sha256）</label>
      <input type="file" name="file" accept=".exe" required>
      <div style="margin-top:14px"><button type="submit">上传并发布</button></div>
    </form>
  </div>
  <div class="card">
    <h2>版本列表</h2>
    <table>
      <tr><th>版本</th><th>大小</th><th>下载</th><th>发布时间</th><th>操作</th></tr>
      %%ROWS%%
    </table>
  </div>
  <div class="card">
    <h2>客户端检查</h2>
    <table>
      <tr><th>累计检查次数</th><td>%%CHECKS%%</td></tr>
      <tr><th>内网地址</th><td><code>%%LAN%%</code></td></tr>
      <tr><th>公网地址</th><td><code>%%PUBLIC%%</code></td></tr>
      <tr><th>客户端清单</th><td><code>%%MANIFEST%%</code></td></tr>
    </table>
  </div>
</div></body></html>"##;

pub fn row(
    id: i64,
    version: &str,
    notes: &str,
    legacy: bool,
    size: i64,
    downloads: i64,
    created_at: &str,
    is_current: bool,
) -> String {
    // 旧版记录即使库里还留着 is_current=1（老库迁移来的），也不该显示"当前"——
    // 它已经发不出去了，标个「当前」只会让人以为客户端还在收它
    let tag = if is_current && !legacy {
        r#" <span class="tag">当前</span>"#
    } else {
        ""
    };
    // 库里还留着已停发的 Python 版历史记录：给个标记，但不再提供"发布"按钮
    let legacy_tag = if legacy {
        r#" <span class="tag py">旧版·已停发</span>"#
    } else {
        ""
    };
    let publish = if is_current || legacy {
        String::new()
    } else {
        format!(
            r#"<form method="post" action="/admin/publish/{id}" style="display:inline"><button class="ghost" type="submit">发布</button></form>"#
        )
    };
    format!(
        r#"<tr><td><b>{version}</b>{tag}{legacy_tag}<br><span class="muted">{notes}</span></td>
<td>{size:.2} MB</td>
<td>{downloads}</td>
<td>{created_at}</td>
<td><div class="row">{publish}<form method="post" action="/admin/delete/{id}" style="display:inline" onsubmit="return confirm('删除 {version}？')"><button class="danger" type="submit">删除</button></form></div></td></tr>"#,
        notes = if notes.is_empty() { "—" } else { notes },
        size = size as f64 / 1024.0 / 1024.0,
    )
}

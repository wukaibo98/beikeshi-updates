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

/* ============================================================ 表单收集页 */

/// 一条提交记录：时间 / 模板 / 人 / 内容（按字段顺序拼成"标签：值"）。
pub fn form_submission_row(
    client_time: &str,
    received_at: &str,
    template_name: &str,
    template_version: i64,
    user_name: &str,
    pairs: &[(String, String)],
) -> String {
    let content = if pairs.is_empty() {
        r#"<span class="muted">—</span>"#.to_string()
    } else {
        pairs
            .iter()
            .map(|(k, v)| {
                format!(
                    r#"<div class="kv"><b>{}</b>：{}</div>"#,
                    crate::escape(k),
                    crate::escape(v)
                )
            })
            .collect::<Vec<_>>()
            .join("")
    };
    format!(
        r#"<tr><td>{client_time}<br><span class="muted">收到 {received_at}</span></td>
<td>{template_name} <span class="muted">v{template_version}</span></td>
<td>{user_name}</td>
<td>{content}</td></tr>"#,
        template_name = crate::escape(template_name),
        user_name = crate::escape(user_name),
    )
}

/// 模板行。应急开关：模板改坏了可以在这里停用它（客户端下次拉取就不再显示）。
pub fn form_template_row(
    id: i64,
    name: &str,
    version: i64,
    field_count: usize,
    updated_at: &str,
    is_active: bool,
) -> String {
    let tag = if is_active {
        r#"<span class="tag">启用中</span>"#
    } else {
        r#"<span class="tag off">已停用</span>"#
    };
    let label = if is_active { "停用" } else { "启用" };
    format!(
        r#"<tr><td><b>{name}</b></td><td>v{version}</td><td>{field_count}</td><td>{updated_at}</td>
<td>{tag}</td>
<td><form method="post" action="/admin/forms/template/toggle" style="display:inline">
<input type="hidden" name="id" value="{id}"><input type="hidden" name="is_active" value="{want}">
<button class="ghost" type="submit">{label}</button></form></td></tr>"#,
        name = crate::escape(name),
        want = if is_active { 0 } else { 1 },
    )
}

pub fn form_user_row(id: i64, name: &str, enabled: bool) -> String {
    let tag = if enabled {
        r#"<span class="tag">可用</span>"#
    } else {
        r#"<span class="tag off">已停用</span>"#
    };
    let label = if enabled { "停用" } else { "启用" };
    format!(
        r#"<tr><td><b>{name}</b></td><td>{tag}</td>
<td><div class="row"><form method="post" action="/admin/users/toggle" style="display:inline">
<input type="hidden" name="id" value="{id}"><input type="hidden" name="enabled" value="{want}">
<button class="ghost" type="submit">{label}</button></form>
<form method="post" action="/admin/users/save" style="display:inline">
<input type="hidden" name="id" value="{id}">
<input type="text" name="name" value="{name}" style="width:120px">
<button class="ghost" type="submit">改名</button></form>
<form method="post" action="/admin/users/delete" style="display:inline"
  onsubmit="return confirm('删除「{name}」？\n\n他填过的登记会保留（记录里已经存了姓名），只是这个人不能再登录了。')">
<input type="hidden" name="id" value="{id}">
<button class="danger" type="submit">删除</button></form></div></td></tr>"#,
        name = crate::escape(name),
        want = if enabled { 0 } else { 1 },
    )
}

/* ================================================ 标签页外壳（v0.3 起） */

pub const PAGE: &str = r##"<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>贝可施更新分发</title>
<style>
*{box-sizing:border-box}
body{margin:0;background:#F4F7EE;color:#232B1C;
  font:14px/1.6 "Microsoft YaHei UI","PingFang SC",system-ui,sans-serif}
.wrap{max-width:1000px;margin:0 auto;padding:28px 20px 60px}
h1{font-size:19px;color:#24400D;margin:0 0 2px}
.sub{font-size:12px;color:#66705C;margin-bottom:16px}
.card{background:#fff;border:1px solid #E2E8D8;border-radius:14px;padding:18px 20px;margin-bottom:14px}
.card h2{font-size:13px;margin:0 0 14px;color:#24400D;font-weight:500}
.card h2 .muted{font-weight:400;margin-left:8px}
label{font-size:12px;color:#66705C}
input[type=text],input[type=password],input[type=date],select,textarea{
  border:1px solid #D8E0CC;border-radius:8px;background:#F8FAF3;padding:6px 8px;
  font:inherit;font-size:13px;outline:none}
textarea{width:100%;resize:vertical;min-height:56px}
input[type=file]{font-size:12px}
button{font:inherit;font-size:13px;height:30px;padding:0 14px;border:0;border-radius:8px;
  background:#3E6B1B;color:#fff;cursor:pointer}
button:hover{background:#335915}
button.ghost{background:transparent;border:1px solid #D8E0CC;color:#66705C;height:28px;padding:0 10px;font-size:12px}
button.ghost:hover{border-color:#3E6B1B;color:#3E6B1B}
button.danger{background:transparent;border:1px solid #F0C0BC;color:#A32D2D;height:28px;padding:0 10px;font-size:12px}
button.danger:hover{background:#FDF4F3}
a.ghost{display:inline-flex;align-items:center;height:28px;padding:0 12px;border-radius:8px;
  border:1px solid #D8E0CC;color:#66705C;text-decoration:none;font-size:12px}
a.ghost:hover{border-color:#3E6B1B;color:#3E6B1B}
a.primary{display:inline-flex;align-items:center;height:30px;padding:0 14px;border-radius:8px;
  background:#3E6B1B;color:#fff;text-decoration:none;font-size:13px}
a.primary:hover{background:#335915}
table{width:100%;border-collapse:collapse;font-size:13px}
th{font-weight:500;color:#66705C;font-size:12px;text-align:left;padding:6px 8px;border-bottom:1px solid #EFF3E7}
td{padding:8px;border-bottom:1px solid #EFF3E7;vertical-align:top}
code{background:#F1F5E9;padding:1px 5px;border-radius:4px;font-size:12px}
.tag{display:inline-block;font-size:11px;padding:1px 7px;border-radius:20px;background:#E7EDDC;color:#3E6B1B}
.tag.off{background:#EFEFEA;color:#98A08F}
.tag.py{background:#EEF1F6;color:#3A5A8C}
.muted{color:#98A08F;font-size:12px}
.flash{background:#EAF3DE;color:#27500A;padding:8px 12px;border-radius:10px;font-size:13px;margin-bottom:14px}
.err{background:#FCEBEB;color:#791F1F;padding:8px 12px;border-radius:10px;font-size:13px;margin-bottom:14px}
.row{display:flex;gap:8px;flex-wrap:wrap;align-items:center}
.top{display:flex;justify-content:space-between;align-items:flex-start;gap:12px}
.kv{font-size:12px}
.kv b{font-weight:500;color:#66705C}
.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:12px}
.stat{background:#F6F9F0;border-radius:10px;padding:12px 14px}
.stat .n{font-size:22px;font-weight:500;color:#24400D;line-height:1.2}
.stat .k{font-size:12px;color:#66705C}

.tabs{display:flex;gap:6px;background:#EDF2E3;border-radius:12px;padding:6px;
  margin-bottom:14px;overflow-x:auto}
.tab{flex:none;display:inline-flex;align-items:center;gap:6px;height:32px;padding:0 16px;
  border-radius:10px;font-size:13px;color:#5F5E5A;text-decoration:none;white-space:nowrap}
.tab:hover{color:#24400D}
.tab.on{background:#fff;color:#24400D;font-weight:500;border:1px solid #CBD9B4}
.badge{font-size:11px;padding:0 6px;border-radius:20px;background:#DCE7C8;color:#3E6B1B;line-height:16px}
.tab.on .badge{background:#E7EDDC}
.dot{display:inline-block;width:8px;height:8px;border-radius:50%;margin-right:6px}
.dot.on{background:#4E8B22}
.dot.off{background:#C9CFC0}
.pane[hidden]{display:none!important}
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
  <nav class="tabs">%%TABS%%</nav>
  <section class="pane" data-pane="overview" %%HIDE_OVERVIEW%%>%%PANE_OVERVIEW%%</section>
  <section class="pane" data-pane="versions" %%HIDE_VERSIONS%%>%%PANE_VERSIONS%%</section>
  <section class="pane" data-pane="register" %%HIDE_REGISTER%%>%%PANE_REGISTER%%</section>
  <section class="pane" data-pane="templates" %%HIDE_TEMPLATES%%>%%PANE_TEMPLATES%%</section>
  <section class="pane" data-pane="users" %%HIDE_USERS%%>%%PANE_USERS%%</section>
</div>
<script>
(function () {
  var tabs = document.querySelectorAll(".tab");
  var panes = document.querySelectorAll(".pane");
  function show(name) {
    for (var i = 0; i < tabs.length; i++) {
      tabs[i].classList.toggle("on", tabs[i].getAttribute("data-tab") === name);
    }
    for (var j = 0; j < panes.length; j++) {
      panes[j].hidden = panes[j].getAttribute("data-pane") !== name;
    }
    var u = new URL(location.href);
    u.searchParams.set("tab", name);
    history.replaceState(null, "", u);
  }
  for (var k = 0; k < tabs.length; k++) {
    tabs[k].addEventListener("click", function (e) {
      if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
      e.preventDefault();
      show(this.getAttribute("data-tab"));
    });
  }
})();
</script>
</body></html>"##;

pub const TABS: [(&str, &str); 5] = [
    ("overview", "概览"),
    ("versions", "版本"),
    ("register", "登记"),
    ("templates", "模板"),
    ("users", "名单"),
];

/// 标签栏。用 `<a href="?tab=">` 而不是按钮 —— 没有 JS 时点它也能整页跳转。
pub fn tabs(active: &str, counts: &[(&str, i64); 5]) -> String {
    TABS.iter()
        .zip(counts.iter())
        .map(|((key, label), (_, count))| {
            let on = if *key == active { " on" } else { "" };
            let badge = if *count > 0 {
                format!(r#"<span class="badge">{count}</span>"#)
            } else {
                String::new()
            };
            format!(
                r#"<a class="tab{on}" href="/admin?tab={key}" data-tab="{key}">{label}{badge}</a>"#
            )
        })
        .collect::<Vec<_>>()
        .join("")
}

/// 概览里"谁在连服务器"的一行。
///
/// 没有心跳，所以"在线"只是"最近 15 分钟内有请求"。时间一律如实显示"N 分钟前"，
/// 不假装在实时在线。
pub fn activity_row(act: &crate::db::Activity, now: i64, tz_offset_min: i32) -> String {
    let online = now - act.at < 15 * 60;
    let dot = if online { "on" } else { "off" };
    let status = if online { "在线" } else { "离线" };
    let who = if act.who.is_empty() {
        r#"<span class="muted">未登录</span>"#.to_string()
    } else {
        crate::escape(&act.who)
    };
    let machine = if act.client_id.is_empty() {
        String::new()
    } else {
        format!(r#"<br><span class="muted">{}</span>"#, crate::escape(&act.client_id))
    };
    let ago = ago_text(now - act.at);
    format!(
        r#"<tr><td><span class="dot {dot}"></span>{who}{machine}</td>
<td><code>{ip}</code></td>
<td>{kind}{detail}</td>
<td>{time}<br><span class="muted">{ago} · {status}</span></td></tr>"#,
        ip = crate::escape(&act.ip),
        kind = crate::db::activity_kind_label(&act.kind),
        detail = if act.detail.is_empty() {
            String::new()
        } else {
            format!(" · {}", crate::escape(&act.detail))
        },
        time = crate::admin::short_time(act.at, tz_offset_min),
    )
}

fn ago_text(secs: i64) -> String {
    if secs < 60 {
        "刚刚".to_string()
    } else if secs < 3600 {
        format!("{} 分钟前", secs / 60)
    } else if secs < 86400 {
        format!("{} 小时前", secs / 3600)
    } else {
        format!("{} 天前", secs / 86400)
    }
}

/// 概览顶部那几个数字。
pub fn stat(key: &str, value: i64) -> String {
    format!(
        r#"<div class="stat"><div class="n">{value}</div><div class="k">{key}</div></div>"#,
        key = crate::escape(key)
    )
}

//! 管理页 HTML。
//!
//! 用 `%%KEY%%` 占位再替换，而不是 `format!` —— 页面里有大量 CSS 花括号，
//! `format!` 会把它们当占位符，写起来满屏 `{{`。原 Python 版也是这么干的。

pub const LOGIN: &str = r##"<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>登录 · 贝可施客服小工具管理系统</title>
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
  <h1>贝可施客服小工具管理系统</h1>
  <p class="sub">请输入管理口令</p>
  <form method="post" action="/admin/login">
    <input type="password" name="token" autofocus required placeholder="管理口令">
    <button type="submit">登录</button>
  </form>
  %%ERROR%%
</div></body></html>"##;

/// 上传记录页的网格增强：把纯文本单元格换成对应类型的控件，改完失焦自动存。
///
/// **控件必须按 `data-kind` 走**，不能一律用文本框 —— 日期要日期选择器、下拉要
/// `<select>`（否则手打一个不在选项里的值，存进去就是脏数据）、数字要数字框、
/// 物流要提示「单号 | 快递公司」的格式。没 JS 时格子是纯文本，编辑走行尾「改」链接。
pub const GRID_SCRIPT: &str = r#"<script>
(function(){
  function setState(tr,text,color){var st=tr.querySelector('.gst');if(st){st.textContent=text;st.style.color=color;}}
  function makeControl(td,card){
    var kind=td.getAttribute('data-kind')||'text';
    var opts=(td.getAttribute('data-opts')||'').split('\u0001');
    var val=td.textContent||'';
    td.textContent='';
    var el;
    if(kind==='select'){
      el=document.createElement('select');
      if(!val){var b=document.createElement('option');b.value='';b.textContent='—';b.selected=true;el.appendChild(b);}
      var has=false;
      for(var i=0;i<opts.length;i++){
        if(opts[i]===''){continue;}
        var o=document.createElement('option');
        o.value=opts[i];o.textContent=opts[i];
        if(opts[i]===val){o.selected=true;has=true;}
        el.appendChild(o);
      }
      if(val&&!has){var e=document.createElement('option');e.value=val;e.textContent=val;e.selected=true;el.appendChild(e);}
    }else{
      el=document.createElement('input');
      if(kind==='date'){el.type='date';}
      else if(kind==='number'){el.type='number';el.step='any';el.setAttribute('inputmode','decimal');}
      else{el.type='text';}
      if(kind==='express'){
        el.title='格式：物流单号 | 物流公司（竖线两边各一个空格）';
        el.placeholder='单号 | 快递公司';
      }
      el.value=val;
    }
    el.className='ginput';
    el.setAttribute('data-key',td.getAttribute('data-key'));
    el.setAttribute('data-orig',val);
    td.appendChild(el);
    el.addEventListener('change',function(){
      var tr=td.closest('tr');
      if(this.value===this.getAttribute('data-orig')){setState(tr,'已存','#3E6B1B');return;}
      setState(tr,'● 未存','#BA7517');
      save(tr,card);
    });
  }
  function save(tr,card){
    var id=tr.getAttribute('data-sub');
    if(!id||!window.fetch){return;}
    var saved=card.querySelector('[data-role="saved"]');
    var err=card.querySelector('[data-role="err"]');
    var data={};
    tr.querySelectorAll('.ginput').forEach(function(el){data[el.getAttribute('data-key')]=el.value;});
    if(err){err.hidden=true;}
    if(saved){saved.hidden=false;saved.textContent='正在保存…';}
    fetch('/admin/forms/submission/grid-save',{method:'POST',credentials:'same-origin',headers:{'Content-Type':'application/json'},body:JSON.stringify({id:Number(id),data:data})})
      .then(function(r){return r.json()})
      .then(function(j){
        if(j&&j.ok){
          tr.querySelectorAll('.ginput').forEach(function(el){el.setAttribute('data-orig',el.value);});
          setState(tr,'✓ '+(j.at||''),'#3E6B1B');
          if(saved){saved.textContent='✓ 已保存 '+(j.at||'');setTimeout(function(){saved.hidden=true;},4000);}
        }else{
          setState(tr,'● 没存上','#A32D2D');
          if(err){err.hidden=false;err.textContent=(j&&j.msg)||'保存失败';}
        }
      })
      .catch(function(){
        setState(tr,'● 没存上','#A32D2D');
        if(err){err.hidden=false;err.textContent='网络不通，这一行还没存上';}
      });
  }
  document.querySelectorAll('.gridcard td.gc[data-key]').forEach(function(td){
    makeControl(td,td.closest('.gridcard'));
  });
})();
</script>"#;

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

/// 模板行。应急开关：模板改坏了可以在这里停用它（客户端下次拉取就不再显示）。
///
/// 「删除」是**软删除**：模板从列表里消失，但历史提交还认得它的名字，
/// 已经导出的表格也不受影响。
pub fn form_template_row(
    id: i64,
    name: &str,
    version: i64,
    field_count: usize,
    updated_at: &str,
    is_active: bool,
    submissions: i64,
) -> String {
    let tag = if is_active {
        r#"<span class="tag">启用中</span>"#
    } else {
        r#"<span class="tag off">已停用</span>"#
    };
    let label = if is_active { "停用" } else { "启用" };
    let used = if submissions > 0 {
        format!(r#"<span class="muted">已有 {submissions} 条登记</span>"#)
    } else {
        r#"<span class="muted">还没有人填过</span>"#.to_string()
    };
    format!(
        r#"<tr><td><b>{name}</b><br>{used}</td><td>v{version}</td><td>{field_count}</td><td>{updated_at}</td>
<td>{tag}</td>
<td><div class="row">
<form method="post" action="/admin/forms/template/toggle" style="display:inline">
<input type="hidden" name="id" value="{id}"><input type="hidden" name="is_active" value="{want}">
<button class="ghost" type="submit">{label}</button></form>
<form method="post" action="/admin/forms/template/delete" style="display:inline"
  onsubmit="return confirm('将模板「{name}」移入已删除模板？\n\n可以之后还原；永久删除时会连同所有填写记录一起删除。')">
<input type="hidden" name="id" value="{id}">
<button class="danger" type="submit">删除</button></form>
</div></td></tr>"#,
        name = crate::escape(name),
        want = if is_active { 0 } else { 1 },
    )
}

/// 「表单预览」里点开某条登记后的编辑表单（也用作弹出层的内容）。
///
/// 字段按**提交那一刻的模板快照**渲染 —— 模板后来改了也不影响这一行的列。
pub fn submission_editor(
    id: i64,
    template_name: &str,
    user_name: &str,
    client_time: &str,
    fields: &[EditorField],
) -> String {
    let rows: String = fields
        .iter()
        .map(|f| {
            let name = format!("f_{}", f.key);
            let control = match f.kind.as_str() {
                "textarea" => format!(
                    r#"<textarea name="{name}" rows="3">{value}</textarea>"#,
                    value = crate::escape(&f.value)
                ),
                "select" => {
                    let opts = f
                        .options
                        .iter()
                        .map(|o| {
                            let sel = if *o == f.value { " selected" } else { "" };
                            format!(
                                r#"<option value="{v}"{sel}>{v}</option>"#,
                                v = crate::escape(o)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    format!(r#"<select name="{name}">{opts}</select>"#)
                }
                "number" => format!(
                    r#"<input type="number" name="{name}" value="{value}" step="any">"#,
                    value = crate::escape(&f.value)
                ),
                "date" => format!(
                    r#"<input type="date" name="{name}" value="{value}">"#,
                    value = crate::escape(&f.value)
                ),
                _ => format!(
                    r#"<input type="text" name="{name}" value="{value}">"#,
                    value = crate::escape(&f.value)
                ),
            };
            format!(
                r#"<div class="frow"><label>{label}</label>{control}</div>"#,
                label = crate::escape(&f.label)
            )
        })
        .collect::<Vec<_>>()
        .join("");
    format!(
        r#"<form method="post" action="/admin/forms/submission/save">
<input type="hidden" name="id" value="{id}">
<div class="kv" style="margin-bottom:10px">
  <b>模板</b>：{template}　<b>提交人</b>：{user}　<b>填写时间</b>：{time}
</div>
{rows}
<div class="row" style="margin-top:14px;justify-content:flex-end">
  <button type="submit">保存修改</button>
</div>
</form>"#,
        template = crate::escape(template_name),
        user = crate::escape(user_name),
        time = crate::escape(client_time),
    )
}

/// 编辑表单里的一个字段。
pub struct EditorField {
    pub key: String,
    pub label: String,
    pub kind: String,
    pub value: String,
    pub options: Vec<String>,
}

/// 「表单预览」网格里的一列。
///
/// 列的来源是**当前模板的字段定义**（不是每条登记自带的快照）——
/// 这样同一模板的所有行才对得齐。老登记里多出来的字段会以 `legacy` 列附在后面。
pub struct GridCol {
    pub key: String,
    pub label: String,
    pub kind: String,
    pub options: Vec<String>,
    /// true = 当前模板已经没有这个字段了（只读，改回去会跟别的行对不上）
    pub legacy: bool,
}

/// 网格的一个单元格。
///
/// **故意渲染成纯文本而不是 input**：没 JS 时它就是只读的文本，编辑入口走
/// 行尾那个「改」链接（整页打开登记编辑表单）；有 JS 时 `bindGrid()` 会把
/// 这些 `<td>` 换成真正的控件。一份 HTML 两种用法，跟模板编辑器同一个套路。
pub fn grid_cell(col: &GridCol, value: &str, editable: bool) -> String {
    if !editable {
        return format!(r#"<td class="gc ro">{}</td>"#, crate::escape(value));
    }
    format!(
        r#"<td class="gc" data-key="{key}" data-kind="{kind}" data-opts="{opts}">{value}</td>"#,
        key = crate::escape(&col.key),
        kind = crate::escape(&col.kind),
        opts = crate::escape(&col.options.join("\u{0001}")),
        value = crate::escape(value),
    )
}

/// 网格的表头：三列只读元数据 + 模板字段 + 状态 + 「改」。
///
/// 提交人 / 填写时间必须给宽度：`table-layout:fixed` 下无宽度的列会平分剩余空间，
/// 姓名三四个字就会被挤成两行。
pub fn grid_head(cols: &[GridCol]) -> String {
    let mut s = String::from(
        r#"<tr><th class="gnum"></th><th style="width:80px">提交人</th><th style="width:98px">填写时间</th>"#,
    );
    for c in cols {
        let tag = if c.legacy {
            r#" <span class="tag off">旧</span>"#
        } else {
            ""
        };
        s.push_str(&format!(
            "<th>{}{}</th>",
            crate::escape(&c.label),
            tag
        ));
    }
    s.push_str(r#"<th class="gst">状态</th><th class="gact">改</th></tr>"#);
    s
}

pub fn form_user_row(id: i64, name: &str, enabled: bool, has_password: bool) -> String {
    let tag = if enabled {
        r#"<span class="tag">可用</span>"#
    } else {
        r#"<span class="tag off">已停用</span>"#
    };
    let label = if enabled { "停用" } else { "启用" };
    format!(
        r#"<tr><td><b>{name}</b><br><span class="muted">{password_status}</span></td><td>{tag}</td>
<td><div class="row"><form method="post" action="/admin/users/toggle" style="display:inline">
<input type="hidden" name="id" value="{id}"><input type="hidden" name="enabled" value="{want}">
<button class="ghost" type="submit">{label}</button></form>
<form method="post" action="/admin/users/save" style="display:inline">
<input type="hidden" name="id" value="{id}">
<input type="text" name="name" value="{name}" style="width:120px">
<button class="ghost" type="submit">改名</button></form>
<form method="post" action="/admin/users/password" style="display:inline">
<input type="hidden" name="id" value="{id}">
<input type="password" name="password" placeholder="新密码（可留空）" maxlength="32">
<input type="password" name="password2" placeholder="确认密码" maxlength="32">
<button class="ghost" type="submit">重置密码</button></form>
<form method="post" action="/admin/users/delete" style="display:inline"
  onsubmit="return confirm('删除「{name}」？\n\n他填过的登记会保留（记录里已经存了姓名），只是这个人不能再登录了。')">
<input type="hidden" name="id" value="{id}">
<button class="danger" type="submit">删除</button></form></div></td></tr>"#,
        name = crate::escape(name),
        want = if enabled { 0 } else { 1 },
        password_status = if has_password { "密码已设置" } else { "空密码（可免密登录）" },
    )
}

/// 管理端人员行。口令**不显示**（库里只存哈希，本来也显示不出来），
/// 要改口令就点「改口令」重新设一个。
pub fn admin_user_row(id: i64, name: &str, enabled: bool, updated_at: &str) -> String {
    let tag = if enabled {
        r#"<span class="tag">可用</span>"#
    } else {
        r#"<span class="tag off">已停用</span>"#
    };
    let label = if enabled { "停用" } else { "启用" };
    format!(
        r#"<tr><td><b>{name}</b></td><td>{tag}</td><td>{updated_at}</td>
<td><div class="row">
<form method="post" action="/admin/admins/toggle" style="display:inline">
<input type="hidden" name="id" value="{id}"><input type="hidden" name="enabled" value="{want}">
<button class="ghost" type="submit">{label}</button></form>
<form method="post" action="/admin/admins/token" style="display:inline">
<input type="hidden" name="id" value="{id}">
<input type="password" name="token" placeholder="新口令" style="width:120px" required>
<button class="ghost" type="submit">改口令</button></form>
<form method="post" action="/admin/admins/delete" style="display:inline"
  onsubmit="return confirm('删除管理员「{name}」？\n\n这个人之后就用这个口令登不进管理页了。')">
<input type="hidden" name="id" value="{id}">
<button class="danger" type="submit">删除</button></form></div></td></tr>"#,
        name = crate::escape(name),
        want = if enabled { 0 } else { 1 },
    )
}

/// 环境变量口令那一条（超级管理员）。它在 compose 的 .env 里，不在库里。
pub fn admin_env_row(configured: bool) -> String {
    if configured {
        r#"<tr><td><b>超级管理员（环境变量口令）</b><br><span class="muted">口令在 NAS 的 .env 里，不在数据库里；改了要重启容器</span></td>
<td><span class="tag">可用</span></td><td>—</td>
<td><span class="muted">不能在这里删除</span></td></tr>"#.to_string()
    } else {
        r#"<tr><td><b>超级管理员（环境变量口令）</b><br><span class="muted">服务端没有配置 ADMIN_TOKEN，这条不能用来登录</span></td>
<td><span class="tag off">未配置</span></td><td>—</td>
<td><span class="muted">在 NAS 的 .env 里设 ADMIN_TOKEN 后重启容器</span></td></tr>"#.to_string()
    }
}

/* ================================================ 模板的网页版编辑器（v0.5 起） */

/// 模板编辑器里的一行字段。
pub struct TplField {
    pub key: String,
    pub label: String,
    pub kind: String,
    pub required: bool,
    pub options: Vec<String>,
    pub group_by_carrier: bool,
}

/// 字段类型的下拉选项。
fn kind_options(selected: &str) -> String {
    crate::forms::KINDS
        .iter()
        .map(|k| {
            let sel = if *k == selected { " selected" } else { "" };
            format!(
                r#"<option value="{k}"{sel}>{label}</option>"#,
                label = crate::forms::kind_label(k)
            )
        })
        .collect()
}

/// 模板的字段编辑器 —— 改的是**表格结构**（有哪些列、每列什么类型）。
///
/// 既是弹出层的内容，也是没 JS 时内联在页面里的那份，所以两种情况都能用：
/// - 位置：数字小的排前面（没 JS 时直接改数字；有 JS 时有 ↑↓ 按钮）
/// - 删：勾上「删」这一行保存时不保留（有 JS 时点一下就把这行收起来）
/// - 加一个字段：没 JS 时点链接整页跳转（重渲染并多一行空字段），
///   有 JS 时在页面上直接克隆一行
pub fn template_editor(id: i64, name: &str, version: i64, fields: &[TplField]) -> String {
    let rows: String = fields
        .iter()
        .enumerate()
        .map(|(i, f)| {
            format!(
                r#"<tr class="tplrow">
<td class="mid"><input type="number" class="tpl-pos" name="f{i}_pos" value="{pos}" min="1" title="位置：数字小的排前面">
<button class="ghost tpl-up" type="button" title="上移">↑</button>
<button class="ghost tpl-down" type="button" title="下移">↓</button></td>
<td><input type="text" name="f{i}_key" value="{key}" placeholder="order_no"></td>
<td><input type="text" name="f{i}_label" value="{label}" placeholder="订单号"></td>
<td><select name="f{i}_type">{kind_opts}</select></td>
<td class="mid"><input type="checkbox" name="f{i}_required" value="1"{req}></td>
<td class="mid"><input type="checkbox" name="f{i}_group_by_carrier" value="1"{group}></td>
<td><input type="text" name="f{i}_options" value="{opts}" placeholder="逗号隔开" title="下拉选项，用逗号隔开"></td>
<td class="mid"><label class="tpl-del"><input type="checkbox" class="tpl-delbox" name="f{i}_del" value="1"> 删</label></td>
</tr>"#,
                pos = i + 1,
                key = crate::escape(&f.key),
                label = crate::escape(&f.label),
                kind_opts = kind_options(&f.kind),
                req = if f.required { " checked" } else { "" },
                group = if f.group_by_carrier { " checked" } else { "" },
                opts = crate::escape(&f.options.join("，")),
            )
        })
        .collect();

    let blank = format!(
        r#"<tr class="tplrow">
<td class="mid"><input type="number" class="tpl-pos" name="f0_pos" value="" min="1">
<button class="ghost tpl-up" type="button">↑</button>
<button class="ghost tpl-down" type="button">↓</button></td>
<td><input type="text" name="f0_key" value=""></td>
<td><input type="text" name="f0_label" value=""></td>
<td><select name="f0_type">{kind_opts}</select></td>
<td class="mid"><input type="checkbox" name="f0_required" value="1"></td>
<td class="mid"><input type="checkbox" name="f0_group_by_carrier" value="1"></td>
<td><input type="text" name="f0_options" value="" title="下拉选项，用逗号隔开"></td>
<td class="mid"><label class="tpl-del"><input type="checkbox" class="tpl-delbox" name="f0_del" value="1"> 删</label></td>
</tr>"#,
        kind_opts = kind_options("text")
    );

    format!(
        r##"<template id="tpl-blank">{blank}</template>
<form method="post" action="/admin/forms/template/save" id="tpl-form">
<input type="hidden" name="id" value="{id}">
<input type="hidden" name="base_version" value="{version}">
<div class="frow"><label>模板名称</label><input type="text" name="name" value="{name}" maxlength="20" required></div>
<p class="muted" style="margin:6px 0 0">保存后版本 +1，客户端下次拉取就是新的字段。
<b>已经填过的登记不受影响</b> —— 每条登记的表格列按提交那一刻的字段定义冻结。</p>
<table class="tpltbl">
<tr><th style="width:120px">位置</th><th style="width:118px">标识</th><th style="width:138px">显示名称</th>
<th style="width:100px">类型</th><th style="width:40px">必填</th><th style="width:105px">按快递分工作表</th><th>下拉选项</th><th style="width:52px">操作</th></tr>
{rows}
</table>
<div class="row" style="margin-top:12px">
  <a class="ghost" href="/admin?tab=register&amp;sub=preview&amp;tpl_edit={id}&amp;add=1" id="tpl-add">加一个字段</a>
  <span class="muted">标识只能用小写字母、数字、下划线；下拉字段至少两个选项</span>
  <button type="submit" style="margin-left:auto">保存</button>
</div>
</form>"##,
        id = id,
        version = version,
        name = crate::escape(name),
        rows = rows,
        blank = blank,
    )
}

/// 「已删除的模板」清单：支持还原和连数据永久删除。
pub fn deleted_template_row(id: i64, name: &str, when: &str, submissions: i64) -> String {
    format!(
        r#"<tr><td><b>{name}</b></td><td>{when}</td><td>{subs} 条填写记录</td>
<td><form method="post" action="/admin/forms/template/restore" style="display:inline"
  onsubmit="return confirm('还原模板「{name}」？')">
<input type="hidden" name="id" value="{id}">
<button class="ghost" type="submit">还原</button></form>
<form method="post" action="/admin/forms/template/purge" style="display:inline">
<input type="hidden" name="id" value="{id}">
<input type="text" name="confirm_name" placeholder="输模板名确认" required
  style="width:112px" autocomplete="off" spellcheck="false"
  title="要一字不差地输入左边的模板名（{name}），才会真的删掉。这是防手滑的闸门，服务端会核对。">
<button class="danger" type="submit">永久删除</button></form></td></tr>"#,
        name = crate::escape(name),
        when = when,
        subs = submissions,
    )
}

/// 「上传记录」新窗口：一个独立页面 —— 没有标签栏，就是一张表。
pub const RECORDS_PAGE: &str = r##"<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>%%TITLE%%</title>
<style>%%CSS%%</style></head>
<body><div class="wrap">
  <div class="top">
    <div>
      <h1>%%TITLE%%</h1>
      <div class="sub">%%SUBTITLE%%</div>
    </div>
    <a class="ghost" href="/admin?tab=register&amp;sub=preview">回到表单预览</a>
  </div>
  %%BODY%%
</div>%%SCRIPT%%</body></html>"##;

/* ================================================ 标签页外壳（v0.4 起两级标签） */

/// 样式：管理页和「上传记录」新窗口共用一份 —— 要改样式只改这一处。
pub const CSS: &str = r##"*{box-sizing:border-box}
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

/* 二级标签：表单登记 / 登录管理 下面各分两页 */
.subtabs{display:flex;gap:4px;border-bottom:1px solid #E2E8D8;margin-bottom:14px;
  padding-bottom:0;overflow-x:auto}
.subtab{flex:none;display:inline-flex;align-items:center;gap:6px;height:30px;padding:0 14px;
  font-size:13px;color:#66705C;text-decoration:none;border-bottom:2px solid transparent;
  margin-bottom:-1px;white-space:nowrap}
.subtab:hover{color:#24400D}
.subtab.on{color:#24400D;font-weight:500;border-bottom-color:#3E6B1B}
.subpane[hidden]{display:none!important}

/* 预览/编辑用的弹出层 */
.sheet{position:fixed;inset:0;background:rgba(35,43,28,.38);display:flex;
  align-items:flex-start;justify-content:center;padding:40px 16px;overflow:auto;z-index:50}
.sheet[hidden]{display:none!important}
.sheet-box{background:#fff;border-radius:14px;width:100%;max-width:720px;
  border:1px solid #E2E8D8;box-shadow:0 12px 40px rgba(35,43,28,.18)}
.sheet-hd{display:flex;justify-content:space-between;align-items:center;gap:12px;
  padding:16px 20px;border-bottom:1px solid #EFF3E7}
.sheet-hd h2{margin:0;font-size:14px;color:#24400D;font-weight:500}
.sheet-bd{padding:16px 20px}
.sheet-ft{display:flex;gap:8px;justify-content:flex-end;padding:14px 20px;
  border-top:1px solid #EFF3E7;background:#FAFCF6;border-radius:0 0 14px 14px}
.frow{display:grid;grid-template-columns:110px 1fr;gap:10px;align-items:center;
  padding:7px 0;border-bottom:1px solid #F3F6EC}
.frow label{font-size:13px;color:#66705C}
.frow input,.frow select,.frow textarea{width:100%}
.frow textarea{min-height:64px}

/* 模板字段编辑器 */
.tpltbl{margin-top:12px}
.tpltbl td{vertical-align:middle;padding:5px 6px}
.tpltbl th{white-space:nowrap}
.tpltbl input[type=text]{width:100%;min-width:64px}
.tpltbl select{width:100%}
.tpltbl input[type=number]{width:100%}
.tpltbl .mid{text-align:center}
.tpl-pos{width:58px;text-align:center}
.tplrow.gone{display:none}
.tplrow.gone td{opacity:.4}
.tplrow td .ghost{height:24px;padding:0 7px;margin-left:3px}
.tpl-del{font-size:12px;color:#A32D2D;white-space:nowrap}
.tpl-del input{width:auto}

/* 表单预览的网格（改登记数据，不是改字段）*/
.gridwrap{overflow-x:auto;border:1px solid #EFF3E7;border-radius:10px}
table.grid{width:100%;border-collapse:collapse;font-size:13px;table-layout:fixed}
table.grid th{font-weight:400;color:#66705C;font-size:12px;text-align:left;
  padding:6px 8px;border-bottom:1px solid #EFF3E7;background:#FAFCF6;white-space:nowrap}
table.grid td{padding:0;border-bottom:1px solid #EFF3E7;vertical-align:middle}
table.grid tr:last-child td{border-bottom:0}
.gnum{width:36px;text-align:center;color:#98A08F;background:#F8FAF3;padding:7px 4px!important}
.gst{width:72px;font-size:12px;color:#3E6B1B;padding:7px 8px!important;white-space:nowrap}
.gact{width:44px;padding:7px 6px!important;text-align:center}
td.ro{background:#F8FAF3;color:#5F5E5A;padding:7px 8px!important}
.gc{padding:0 4px}
.gc input,.gc select{width:100%;border:1px solid transparent;background:transparent;
  border-radius:6px;padding:6px 7px;font:inherit;font-size:13px;color:#232B1C;outline:none}
.gc input:hover,.gc select:hover{border-color:#D8E0CC;background:#fff}
.gc input:focus,.gc select:focus{border-color:#3E6B1B;background:#F6F9F0}
.dot{display:inline-block;width:7px;height:7px;border-radius:50%;background:#EF9F27;margin-right:4px}
.dot[hidden]{display:none}
.gridfoot{display:flex;gap:10px;align-items:center;margin-top:10px;flex-wrap:wrap}
.gsave{background:#EAF3DE;color:#27500A;border-radius:10px;padding:5px 12px;font-size:12px}
.gerr{background:#FCEBEB;color:#791F1F;border-radius:10px;padding:5px 12px;font-size:12px}
"##;

pub const PAGE: &str = r##"<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>贝可施客服小工具管理系统</title>
<style>%%CSS%%</style></head>
<body><div class="wrap">
  <div class="top">
    <div>
      <h1>贝可施客服小工具管理系统</h1>
      <div class="sub">%%SUBTITLE%%</div>
    </div>
    <form method="post" action="/admin/logout"><button class="ghost" type="submit">退出</button></form>
  </div>
  %%FLASH%%
  <nav class="tabs">%%TABS%%</nav>
  <section class="pane" data-pane="overview" %%HIDE_OVERVIEW%%>%%PANE_OVERVIEW%%</section>
  <section class="pane" data-pane="register" %%HIDE_REGISTER%%>
    <nav class="subtabs">%%SUB_REGISTER%%</nav>
    <div class="subpane" data-spane="preview" %%HIDE_REG_PREVIEW%%>%%PANE_REG_PREVIEW%%</div>
    <div class="subpane" data-spane="templates" %%HIDE_REG_TEMPLATES%%>%%PANE_REG_TEMPLATES%%</div>
  </section>
  <section class="pane" data-pane="versions" %%HIDE_VERSIONS%%>%%PANE_VERSIONS%%</section>
  <section class="pane" data-pane="login" %%HIDE_LOGIN%%>
    <nav class="subtabs">%%SUB_LOGIN%%</nav>
    <div class="subpane" data-spane="clients" %%HIDE_LOGIN_CLIENTS%%>%%PANE_LOGIN_CLIENTS%%</div>
    <div class="subpane" data-spane="admins" %%HIDE_LOGIN_ADMINS%%>%%PANE_LOGIN_ADMINS%%</div>
  </section>
</div>

<!-- 预览/编辑某条登记用的弹出层 -->
<div class="sheet" id="sheet" hidden>
  <div class="sheet-box">
    <div class="sheet-hd">
      <h2 id="sheet-title">预览</h2>
      <button class="ghost" id="sheet-close" type="button">关闭</button>
    </div>
    <div class="sheet-bd" id="sheet-body"></div>
  </div>
</div>
<script>
(function () {
  var tabs = document.querySelectorAll(".tab");
  var panes = document.querySelectorAll(".pane");

  function showSub(pane, subName) {
    var subs = pane.querySelectorAll(".subtab");
    var spanes = pane.querySelectorAll(".subpane");
    for (var i = 0; i < subs.length; i++) {
      subs[i].classList.toggle("on", subs[i].getAttribute("data-sub") === subName);
    }
    for (var j = 0; j < spanes.length; j++) {
      spanes[j].hidden = spanes[j].getAttribute("data-spane") !== subName;
    }
  }

  function currentSub(pane) {
    var s = pane && pane.querySelector(".subtab.on");
    return s ? s.getAttribute("data-sub") : "";
  }

  function setUrl(tab, sub) {
    var u = new URL(location.href);
    u.searchParams.set("tab", tab);
    if (sub) { u.searchParams.set("sub", sub); } else { u.searchParams.delete("sub"); }
    history.replaceState(null, "", u);
  }

  function showTop(name) {
    for (var i = 0; i < tabs.length; i++) {
      tabs[i].classList.toggle("on", tabs[i].getAttribute("data-tab") === name);
    }
    var active = null;
    for (var j = 0; j < panes.length; j++) {
      var on = panes[j].getAttribute("data-pane") === name;
      panes[j].hidden = !on;
      if (on) {
        active = panes[j];
        var s = panes[j].querySelector(".subtab.on") || panes[j].querySelector(".subtab");
        if (s) { showSub(panes[j], s.getAttribute("data-sub")); }
      }
    }
    setUrl(name, active ? currentSub(active) : "");
  }

  for (var k = 0; k < tabs.length; k++) {
    tabs[k].addEventListener("click", function (e) {
      if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
      e.preventDefault();
      showTop(this.getAttribute("data-tab"));
    });
  }

  var subtabs = document.querySelectorAll(".subtab");
  for (var m = 0; m < subtabs.length; m++) {
    subtabs[m].addEventListener("click", function (e) {
      if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
      e.preventDefault();
      var pane = this.closest(".pane");
      if (!pane) return;
      showSub(pane, this.getAttribute("data-sub"));
      setUrl(pane.getAttribute("data-pane"), this.getAttribute("data-sub"));
    });
  }

  /* ---- 预览/编辑某条登记 ----
     有 JS：点「预览」只取一段 HTML 塞进弹出层，不整页刷新。
     没 JS：点「预览」走普通链接，服务端把编辑表单渲染在预览区顶部，一样能用。 */
  var sheet = document.getElementById("sheet");
  var sheetBody = document.getElementById("sheet-body");
  var sheetTitle = document.getElementById("sheet-title");
  var editorHost = document.getElementById("rec-editor-host");

  function openSheet(html, title) {
    if (!sheet || !sheetBody) return;
    sheetBody.innerHTML = html;
    if (sheetTitle && title) { sheetTitle.textContent = title; }
    sheet.hidden = false;
  }

  function closeSheet() {
    if (!sheet) return;
    sheet.hidden = true;
    sheetBody.innerHTML = "";
    // 顺手把地址栏里的 rec= / tpl_edit= 清掉，刷新不会又弹出来
    var u = new URL(location.href);
    u.searchParams.delete("rec");
    u.searchParams.delete("tpl_edit");
    history.replaceState(null, "", u);
  }

  if (sheet && sheetBody) {
    // 页面自带一份编辑表单（无 JS 走这条路的产物）：挪进弹出层
    var inline = document.getElementById("rec-editor");
    if (inline) {
      sheetBody.appendChild(inline);
      sheet.hidden = false;
    }
    var inlineTpl = document.getElementById("tpl-editor");
    if (inlineTpl) {
      sheetBody.appendChild(inlineTpl);
      sheet.hidden = false;
    }
    var close = document.getElementById("sheet-close");
    if (close) { close.addEventListener("click", closeSheet); }
    sheet.addEventListener("click", function (e) {
      if (e.target === sheet) { closeSheet(); }
    });
    document.addEventListener("keydown", function (e) {
      if (e.key === "Escape" && !sheet.hidden) { closeSheet(); }
    });
  }

  /* ---- 模板字段编辑器（改表格结构）----
     有 JS：点「编辑」只取一段 HTML 塞进弹出层，页面上直接加行/删行/上下移。
     没 JS：点「编辑」走普通链接，服务端把编辑器渲染在预览区顶部，一样能用
     （位置改数字、勾「删」、点「加一个字段」整页跳一下）。 */
  function tplRows() {
    var f = document.getElementById("tpl-form");
    return f ? f.querySelectorAll(".tplrow") : [];
  }

  function renumberTpl() {
    var rows = tplRows();
    for (var i = 0; i < rows.length; i++) {
      var inputs = rows[i].querySelectorAll("[name]");
      for (var j = 0; j < inputs.length; j++) {
        var nm = inputs[j].getAttribute("name") || "";
        if (nm.indexOf("f") === 0) {
          inputs[j].setAttribute("name", "f" + i + "_" + nm.replace(/^f\d+_/, ""));
        }
      }
      var pos = rows[i].querySelector(".tpl-pos");
      if (pos && !pos.value) { pos.value = String(i + 1); }
    }
  }

  function bindTplEditor() {
    var form = document.getElementById("tpl-form");
    if (!form) { return; }
    renumberTpl();

    var add = document.getElementById("tpl-add");
    if (add && !add.getAttribute("data-bound")) {
      add.setAttribute("data-bound", "1");
      add.addEventListener("click", function (e) {
        if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
        var tpl = document.getElementById("tpl-blank");
        if (!tpl) { return; }
        e.preventDefault();
        var table = form.querySelector(".tpltbl");
        if (!table) { return; }
        table.appendChild(tpl.content.cloneNode(true));
        renumberTpl();
      });
    }

    var ups = form.querySelectorAll(".tpl-up");
    for (var u = 0; u < ups.length; u++) {
      if (ups[u].getAttribute("data-bound")) { continue; }
      ups[u].setAttribute("data-bound", "1");
      ups[u].addEventListener("click", function () {
        var row = this.closest(".tplrow");
        var prev = row && row.previousElementSibling;
        if (!prev || !prev.classList.contains("tplrow")) { return; }
        var a = row.querySelector(".tpl-pos");
        var b = prev.querySelector(".tpl-pos");
        if (a && b) { var t = a.value; a.value = b.value; b.value = t; }
      });
    }

    var downs = form.querySelectorAll(".tpl-down");
    for (var d = 0; d < downs.length; d++) {
      if (downs[d].getAttribute("data-bound")) { continue; }
      downs[d].setAttribute("data-bound", "1");
      downs[d].addEventListener("click", function () {
        var row = this.closest(".tplrow");
        var next = row && row.nextElementSibling;
        if (!next || !next.classList.contains("tplrow")) { return; }
        var a = row.querySelector(".tpl-pos");
        var b = next.querySelector(".tpl-pos");
        if (a && b) { var t = a.value; a.value = b.value; b.value = t; }
      });
    }

    var dels = form.querySelectorAll(".tpl-delbox");
    for (var x = 0; x < dels.length; x++) {
      if (dels[x].getAttribute("data-bound")) { continue; }
      dels[x].setAttribute("data-bound", "1");
      dels[x].addEventListener("change", function () {
        var row = this.closest(".tplrow");
        if (row) { row.classList.toggle("gone", this.checked); }
      });
    }
  }

  if (document.getElementById("tpl-form")) { bindTplEditor(); }

  var recLinks = document.querySelectorAll(".rec-open");
  for (var n = 0; n < recLinks.length; n++) {
    recLinks[n].addEventListener("click", function (e) {
      if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
      if (!sheet || !window.fetch) return;
      e.preventDefault();
      var id = this.getAttribute("data-id");
      var title = this.getAttribute("data-title") || "预览";
      var href = this.getAttribute("href");
      fetch("/admin/forms/submission/" + id + "/edit", { credentials: "same-origin" })
        .then(function (r) {
          if (!r.ok) { throw new Error("HTTP " + r.status); }
          return r.text();
        })
        .then(function (html) { openSheet(html, title); })
        .catch(function () { location.href = href; });
    });
  }

  /* ---- 编辑模板（改表格结构）----
     有 JS：点「编辑」取一段 HTML 塞进弹出层，页面上直接改。
     没 JS：点「编辑」走普通链接，服务端把编辑器渲染在预览区顶部，一样能用。 */
  var tplLinks = document.querySelectorAll(".tpl-open");
  for (var p = 0; p < tplLinks.length; p++) {
    tplLinks[p].addEventListener("click", function (e) {
      if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
      if (!sheet || !window.fetch) return;
      e.preventDefault();
      var id = this.getAttribute("data-tpl");
      var title = this.getAttribute("data-title") || "编辑模板";
      var href = this.getAttribute("href");
      fetch("/admin/forms/template/" + id + "/edit", { credentials: "same-origin" })
        .then(function (r) {
          if (!r.ok) { throw new Error("HTTP " + r.status); }
          return r.text();
        })
        .then(function (html) { openSheet(html, title); bindTplEditor(); })
        .catch(function () { location.href = href; });
    });
  }
  /* ---- 表单预览的网格（改登记数据）----
     有 JS：把文本单元格换成 input/select/date，改完失焦自动存。
     没 JS：这些格子就是纯文本，编辑入口是行尾那个「改」链接
     （整页打开登记编辑表单，服务端照样能存）。 */
  function gridMarkDirty(tr, on) {
    var dot = tr.querySelector(".dot");
    var st = tr.querySelector(".gst");
    if (dot) { dot.hidden = !on; }
    if (st && on) { st.textContent = "● 未存"; st.style.color = "#BA7517"; }
  }

  function gridMakeEditable(td, card) {
    var kind = td.getAttribute("data-kind") || "text";
    var opts = (td.getAttribute("data-opts") || "").split("\u0001");
    var val = td.textContent || "";
    td.textContent = "";
    var el;
    if (kind === "select") {
      el = document.createElement("select");
      if (!val) {
        var blank = document.createElement("option");
        blank.value = ""; blank.textContent = "—"; blank.selected = true;
        el.appendChild(blank);
      }
      var has = false;
      for (var o = 0; o < opts.length; o++) {
        if (opts[o] === "") { continue; }
        var op = document.createElement("option");
        op.value = opts[o]; op.textContent = opts[o];
        if (opts[o] === val) { op.selected = true; has = true; }
        el.appendChild(op);
      }
      if (val && !has) {
        var extra = document.createElement("option");
        extra.value = val; extra.textContent = val; extra.selected = true;
        el.appendChild(extra);
      }
    } else {
      el = document.createElement("input");
      if (kind === "date") { el.type = "date"; }
      else if (kind === "number") { el.type = "number"; el.step = "any"; el.inputMode = "decimal"; }
      else { el.type = "text"; }
      el.value = val;
    }
    el.className = "ginput";
    el.setAttribute("data-key", td.getAttribute("data-key"));
    el.setAttribute("data-orig", val);
    td.appendChild(el);
    el.addEventListener("change", function () {
      var tr = td.parentNode;
      if (this.value === this.getAttribute("data-orig")) { gridMarkDirty(tr, false); return; }
      gridMarkDirty(tr, true);
      gridSaveRow(tr, card);
    });
  }

  function gridRowData(tr) {
    var inputs = tr.querySelectorAll(".ginput");
    var d = {};
    for (var i = 0; i < inputs.length; i++) {
      d[inputs[i].getAttribute("data-key")] = inputs[i].value;
    }
    return d;
  }

  function gridSaveRow(tr, card) {
    var id = tr.getAttribute("data-sub");
    if (!id || !window.fetch) { return; }
    var saved = card.querySelector('[data-role="saved"]');
    var err = card.querySelector('[data-role="err"]');
    if (err) { err.hidden = true; }
    if (saved) { saved.hidden = false; saved.textContent = "正在保存…"; }
    fetch("/admin/forms/submission/grid-save", {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ id: parseInt(id, 10), data: gridRowData(tr) })
    })
      .then(function (r) { return r.json(); })
      .then(function (j) {
        var st = tr.querySelector(".gst");
        if (j && j.ok) {
          gridMarkDirty(tr, false);
          var inputs = tr.querySelectorAll(".ginput");
          for (var i = 0; i < inputs.length; i++) {
            inputs[i].setAttribute("data-orig", inputs[i].value);
          }
          if (st) { st.textContent = "✓ " + (j.at || ""); st.style.color = "#3E6B1B"; }
          if (saved) { saved.textContent = "✓ 已保存 " + (j.at || ""); }
          window.setTimeout(function () { if (saved) { saved.hidden = true; } }, 4000);
        } else {
          if (st) { st.textContent = "● 没存上"; st.style.color = "#A32D2D"; }
          if (err) { err.hidden = false; err.textContent = (j && j.msg) || "保存失败"; }
        }
      })
      .catch(function () {
        var st2 = tr.querySelector(".gst");
        if (st2) { st2.textContent = "● 没存上"; st2.style.color = "#A32D2D"; }
        if (err) { err.hidden = false; err.textContent = "网络不通，这一行还没存上"; }
      });
  }

  function bindGrid() {
    var cards = document.querySelectorAll(".gridcard");
    for (var c = 0; c < cards.length; c++) {
      var cells = cards[c].querySelectorAll("td.gc[data-key]");
      for (var i = 0; i < cells.length; i++) {
        gridMakeEditable(cells[i], cards[c]);
      }
    }
  }

  bindGrid();
})();
</script>
</body></html>"##;

pub const TABS: [(&str, &str); 4] = [
    ("overview", "概览"),
    ("register", "表单登记"),
    ("versions", "版本更新"),
    ("login", "登录管理"),
];

/// 顶层标签下面的二级标签。没有二级标签的标签（概览、版本更新）不在表里。
pub const SUBTABS: &[(&str, &[(&str, &str)])] = &[
    ("register", &[("preview", "表单预览"), ("templates", "模板管理")]),
    ("login", &[("clients", "客户端"), ("admins", "管理端")]),
];

/// 某个顶层标签下的默认二级标签（URL 没给 `sub` 时用这个）。
pub fn default_sub(tab: &str) -> &'static str {
    SUBTABS
        .iter()
        .find(|(t, _)| *t == tab)
        .and_then(|(_, subs)| subs.first())
        .map(|(k, _)| *k)
        .unwrap_or("")
}

/// 标签栏。用 `<a href="?tab=">` 而不是按钮 —— 没有 JS 时点它也能整页跳转。
pub fn tabs(active: &str, counts: &[(&str, i64); 4]) -> String {
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

/// 二级标签栏。同样用 `<a href>`，无 JS 时也能整页跳转。
pub fn sub_tabs(top: &str, active_sub: &str) -> String {
    let Some((_, subs)) = SUBTABS.iter().find(|(t, _)| *t == top) else {
        return String::new();
    };
    subs.iter()
        .map(|(key, label)| {
            let on = if *key == active_sub { " on" } else { "" };
            format!(
                r#"<a class="subtab{on}" href="/admin?tab={top}&sub={key}" data-sub="{key}">{label}</a>"#
            )
        })
        .collect::<Vec<_>>()
        .join("")
}

/// 概览顶部那几个数字。
pub fn stat(key: &str, value: i64) -> String {
    format!(
        r#"<div class="stat"><div class="n">{value}</div><div class="k">{key}</div></div>"#,
        key = crate::escape(key)
    )
}

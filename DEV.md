# 贝可施更新分发服务 — 开发文档

> 面向改这个服务的人。部署与日常发版见 [README.md](README.md)，交接要点与待办见 [HANDOVER.md](HANDOVER.md)。

**服务端 v0.3.1 ｜ Rust 1.75+ / axum 0.8 / rusqlite(bundled) / tokio ｜ 静态二进制 → scratch 镜像**
**代码 1172 行（main 327 / admin 389 / db 206 / html 159 / auth 91）**

---

## 1. 技术选型

| 项 | 选择 | 理由 |
|---|---|---|
| 语言 | Rust（edition 2021，`rust-version = 1.75`） | 部署形态要"扔上去四年不用管"。无运行时、无 GC、内存 ~5MB |
| Web | axum 0.8 | tokio 原生、路由与 extractor 够用，不需要中间件生态 |
| 存储 | SQLite（rusqlite `bundled`） | SQLite 源码一起编进去，静态二进制不依赖系统 libsqlite3 |
| 异步 | tokio（rt-multi-thread + fs + io-util） | 上传/下载都走流式，靠 `ReaderStream` / `Field::chunk` |
| 模板 | 手写 HTML + `%%KEY%%` 占位替换 | 管理页就一屏，引模板引擎反而多一层依赖（见 §7 的坑） |
| 镜像 | 多阶段 → **scratch** | 静态 musl 二进制，最终层只有一个文件 |

`Cargo.toml` 的 release profile 特意调成"体积优先"：

```toml
opt-level = "z"   # 不是 3：这服务 CPU 不是瓶颈，体积才是（镜像 4MB）
lto = true
codegen-units = 1
strip = true
panic = "abort"
```

产物 **1,927,872 字节（1.84MB）**，`panic = "abort"` 顺带让二进制更小
（代价：捕获不到 panic 展开，这服务也不需要）。

## 2. 模块地图

```
main.rs   ── 配置读取（环境变量）→ 建 DATA_DIR/packages → 开库 → 载/建签名密钥
          ├─ 路由表
          ├─ GET /latest.json      清单生成（唯一涉及两条版本线的地方）
          ├─ GET /packages/{name}  文件白名单 + 流式下发 + 下载计数
          ├─ --health / --version  子命令（scratch 里没有 shell/curl）
          └─ detect_package()      Inno 标记检测（头尾各 4MB）
admin.rs  ── GET  /admin            页面渲染（未登录 → 登录页）
          ├─ POST /admin/login     口令校验 + 发签名 Cookie
          ├─ POST /admin/upload    流式落盘 + sha256 + 入库
          ├─ POST /admin/publish/{id}  回滚
          └─ POST /admin/delete/{id}   删除
db.rs     ── 全部 SQL；表结构与 Python 版兼容（client 列只标记旧记录）
          ├─ 表单四表 + admin_users（v0.4 新增）
          ├─ 模板软删除（form_template_delete）
          └─ 模板永久删除（form_template_purge，v0.5 新增）
auth.rs   ── HMAC-SHA256 签名：管理 Cookie（带管理员 id，v0.4）/ 表单 token / 编辑 token
forms.rs  ── schema 校验、姓名登录、模板存取（乐观锁）、提交（幂等+复验）
          ├─ GET  /admin/forms            表单管理页（v0.4 起渲染成四个二级面板）
          ├─ GET  /admin/forms/export.xlsx 导出（一次只导一个模板）
          ├─ POST /admin/forms/template/delete        模板软删除
          ├─ GET  /admin/forms/template/{id}/edit     模板编辑器片段（v0.5）
          ├─ GET  /admin/forms/template/{id}/records  上传记录独立页（v0.5）
          ├─ POST /admin/forms/template/save         模板保存（v0.5）
          ├─ POST /admin/forms/template/purge        模板永久删除（v0.5）
          ├─ GET  /admin/forms/submission/{id}/edit   登记编辑片段（弹层用）
          ├─ POST /admin/forms/submission/save        登记保存（只改 data_json）
          ├─ POST /admin/forms/submission/grid-save   网格失焦即存（v0.6，JSON）
          └─ POST /admin/admins/{save,toggle,token,delete} 管理端人员与口令
xlsx.rs   ── rust_xlsxwriter：列并集 → 数字列 → sheet 名清洗
html.rs   ── 登录页 / 分发页 / 表单页模板（`%%占位%%`）；v0.4 加两级标签结构；
          v0.5 加 CSS 抽出（pub const CSS）、模板编辑器模板（template_editor）、
          已删除模板行（deleted_template_row）、上传记录独立页（RECORDS_PAGE）；
          v0.6 加网格列模型（GridCol）、单元格/表头/空行（grid_cell / grid_head /
          grid_empty_row）、网格的 JS 增强（bindGrid，见 PAGE 尾部）
tests.rs  ── `cargo test`：幂等、快照冻结、乐观锁、白名单、导出列、网格改登记
tools/shot-admin.js ── 无头 Chrome 截管理页（CDP 注入自签 Cookie，见 §9.3）
tools/grid-cdp-test.js ── 网格交互实测（12 项，见 §9.4）
```

数据流很短，没有 service 层也没有 repository 抽象 —— 这个规模不需要：

```
浏览器 → admin.rs（解析/校验/落盘） → db.rs（一条 SQL） → html.rs（拼页面）
客户端 → main.rs（取当前版本） → db.rs → serde_json → 返回
```

## 3. 数据模型

```sql
CREATE TABLE releases (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    version    TEXT NOT NULL,             -- "0.2.0"，不带 v 前缀
    filename   TEXT NOT NULL,             -- 服务端拼的："bkstools-0.2.0.exe"
    size       INTEGER NOT NULL,          -- 字节
    sha256     TEXT NOT NULL,             -- 上传时边写边算
    notes      TEXT DEFAULT '',           -- 更新说明（会显示在客户端更新条）
    created_at INTEGER NOT NULL,          -- Unix 秒
    downloads  INTEGER DEFAULT 0,
    is_current INTEGER DEFAULT 0,         -- 1 = 当前对外发布的版本（只有一条线）
    package    TEXT DEFAULT 'portable',   -- installer / portable
    client     TEXT DEFAULT 'python'      -- 仅用于标记历史：'web' = 在发，其它 = 旧版已停发
);

CREATE TABLE checks (                     -- 单行表：客户端检查次数的累计
    id      INTEGER PRIMARY KEY CHECK (id = 1),
    count   INTEGER DEFAULT 0,
    last_at INTEGER DEFAULT 0
);
```

几条语义要记牢：

- **只有一条版本线**：`db::CLIENT = "web"`。`is_current` 只对这条线有意义，
  更新它的 SQL 仍带 `WHERE client = ?` —— 库里还有已停发的 `python` 老记录，
  不加条件会把它们的历史标记一起清掉（图上不明显，回滚时才发现）。
- **旧记录不能被切回当前**：`db::publish` 返回三态（`Published` / `NotFound` / `Legacy`），
  命中旧版就直接拒。放开了会出现"当前版本指向一个谁也拿不到的线上"，
  等于所有客户端停止收更新。
- **`filename` 由服务端拼**，不采信上传上来的名字，固定 `bkstools-<版本>.exe`。
- **老库升级靠两行 `ALTER TABLE ... ADD COLUMN`**（`package` / `client`），
  失败就说明列已存在，用 `let _ =` 忽略。**别删**，NAS 上那份库是旧版建的。
- `journal_mode = WAL` + `synchronous = NORMAL`：读不挡写，二十个客服同时查更新不互相等；
  断电最多丢最后几条计数，版本记录本身不会坏。

### 3.1 表单的四张表（v0.2 起）

```sql
form_users (id, name UNIQUE, password_hash, auth_version, enabled, created_at) -- 姓名账号、Argon2 密码哈希、令牌版本
form_templates (id, name UNIQUE, version, is_active, sort_order, updated_at,
                deleted_at)                           -- v0.4 加 deleted_at，软删除用
form_template_versions (template_id, version, schema_json, created_at)  -- 每个版本的字段定义
form_submissions (id, submission_id UNIQUE, template_id, template_version,
                  template_snapshot, user_id, user_name, data_json,
                  client_time, created_at, client_id,
                  template_name)              -- v0.5 加：模板名冗余，永久删除后仍认得出表名
```

关键语义：

- **`template_snapshot` 是提交那一刻的字段定义，冻结不动。**
  模板一改，历史提交的列就变了；导出按每条提交自带的快照取列，永不串行。
  配套：`form_template_versions` 保存**每个版本**的定义，旧的也能取回来。
- **`template_name` 是模板名的冗余副本（v0.5）**。软删除时模板行还在，名字从
  `form_templates.name` 取就够；但**永久删除**会把模板行真删掉，历史登记就会变成
  一个认不出表名的孤儿。所以在永久删除那一刻，**先把模板名写进这一列**再删行 ——
  顺序不能反（先删行就拿不到名字了）。
  - 这一列在 `form_submissions` 里是 `NOT NULL DEFAULT ''`，
    老库靠 `ALTER TABLE ... ADD COLUMN template_name TEXT NOT NULL DEFAULT ''` 补上。
  - 加列之前提交的老登记，这一列是**空串** —— 这是预期的，
    它们本来就在模板行还完整的时候提交过，名字随时能从 `form_templates` 取到；
    只有走永久删除时才会被补上。
- **`submission_id UNIQUE` + `ON CONFLICT DO NOTHING`** 是幂等的命根子。
  客户端生成一次，所有重试用同一个；服务端返回 `duplicate` 也算成功
  （客户端只有看到成功才会清队列，网络超时其实已入库时重试不会产生第二条）。
- **`client_time` 和 `created_at` 分开**：离线补交的单子可能几小时后才到，
  报表一律按"客服填的时间"排，服务器收到的时间只用于排查。
- **`user_name` 冗余存一份**：改名/删人后历史记录还认得出是谁填的。
- 四张表都在启动时的 `init_forms()` 里 `CREATE TABLE IF NOT EXISTS`，
  NAS 上那份老库原地加表，不用迁移脚本。
- **`deleted_at = 0` 表示没删**（不是 NULL）。软删除见 §4.12。

### 3.2 管理端人员表（v0.4 起）

```sql
admin_users (id, name, token_hash, enabled, created_at, updated_at)
```

管理端账号与客户端白名单是**两套东西**：

- `form_users` 管客服账号：姓名作为账号，密码使用 Argon2 哈希；`auth_version` 让改密、改名、停用/启用都能撤销旧会话。
- `admin_users` 管的是"谁能进管理页"，凭口令登录，口令**只存 sha256 哈希**。

`admin_users` 与 `form_users` 互不干涉 —— 客服不在管理端名单里照样能填表，
管理员不在客服白名单里也能进后台。环境变量 `ADMIN_TOKEN` 保留为**超级管理员**
（内部 id = 0），两个入口都认。

## 4. 接口实现细节

### 4.1 `GET /latest.json?client=web`

```
client 缺省  → 404 {"error":"旧版客户端已停止发布，请换用新版"}
client ≠ web → 404 {"error":"未知客户端：xx"}
client = web → 按 (is_current=1, client='web') 取一行；没有 → 404 {"error":"还没有发布任何版本"}
拼 windows_x64: {size, sha256, lan_url?, public_url?}
响应头一律 Cache-Control: no-store
```

**对旧客户端回 404 是刻意设计**，不是懒：旧 Python 客户端的更新逻辑里没有
`client` 校验，一旦把新版的清单发给它，它会把 `bkstools-*.exe` 当自己的新版下载下来、
替换掉自己（两个包都是 `.exe`，它分辨不出来）。404 让它在自己那边显示
"服务器还没有发布版本"，安静地不更新，是最安全的结局。

三个容易出事的点：

1. **清单每次现生成，不缓存**。发布、回滚立刻生效，也不会出现"清单和数据库不一致"。
   所以客户端和服务端都不该加缓存层；已经加了 `no-store`。
2. **`lan_url` / `public_url` 是可选字段**：`LAN_BASE` 为空就不出现。
   两个都空时退化成相对路径（`"/packages/x.exe"`），客户端能解析但跨网用不了 ——
   这是为了"本地跑起来也能测清单结构"。
3. **404 是有语义的**，不是故障。客户端据此区分"服务活着但没发版"和"连不上"。
   改动时别把它改成 200 + 空对象，也别并进 500。

同时 `db::bump_checks()` 会累加检查次数（管理页那个"累计检查次数"）。

### 4.2 `GET /packages/{name}`

```
字符合集校验：ascii_alphanumeric + '.' '_' '-'，其它一律 400
   → 拼 pkg_dir/name，metadata 失败 → 404
   → 查库看这个文件对应哪个版本，downloads += 1
   → ReaderStream + Content-Length + Content-Disposition 流式下发
```

- 字符集白名单是**唯一**的防穿越手段（先把 `/`、`\`、`..` 从字符层面掐掉），
  比事后规范化 `canonicalize` 更省心。
- 流式：大包不会把服务端内存吃满（内网 67MB 的 exe 实测没问题）。
- 下载计数是"尽力而为"：写库失败不影响下发（`let _ =`）。

#### 请求体大小限制（v0.3.1 修的坑）

axum 的 **`DefaultBodyLimit` 默认是 2MB**。64MB 的安装包传到 2MB 就被截断，
`Multipart::chunk()` 直接返回错误，浏览器看到的是**"连接被重置"**，
而不是一句"文件太大" —— 排查时极易误判成网络问题。

修法是在 Router 上放宽到略大于单包上限（300MB + 20MB）：

```rust
.layer(DefaultBodyLimit::max((admin::MAX_UPLOAD_BYTES + 20 * 1024 * 1024) as usize))
```

另外三条配套：

- **错误路径必须删临时文件**。中断时会在 `packages/` 留下 `.upload-<ts>.tmp` 半个文件，
  不删就一直堆着。
- **启动时清一次** `.upload-*.tmp`（进程被打断时它们删不掉）。
- **小于 64KB 直接拒**：安装包不可能这么小，出现就说明传断了，宁可拒绝也不发布残缺包。

### 4.3 `POST /admin/upload`

```
multipart 逐字段读：
  version → 先校验格式（≥2 段，每段纯数字），不对立刻拒
  notes   → 原样存
  client  → 已废弃，收到也丢掉（老表单还带着这个字段）
  file    → 流式写到 packages/.upload-<时间戳>.tmp，边写边算 sha256
            超过 300MB 中止并删临时文件；0 字节拒绝
  → rename 到最终文件名（服务端拼）
  → detect_package() 判断 installer / portable
  → insert_release()：本线的旧 is_current 清 0，新记录置 1
  → 303 重定向回 /admin?ok=<百分号编码的中文消息>
  → 任何一步失败：删掉已落盘的文件，303 到 /admin?err=...
```

**上传即发布**，没有"草稿"状态。理由是发版频率极低（一周一次都不到），
少一个状态就少一种"忘了点发布"的故障。

失败要把文件删掉，否则 `packages/` 里会慢慢攒下一堆没有数据库记录的孤儿文件。

### 4.4 `POST /admin/publish/{id}` / `delete/{id}`

- publish：先 `UPDATE ... is_current=0 WHERE client='web'`，再把自己置 1；命中旧版记录直接拒绝；
  这就是"回滚"——不需要额外的版本序列号。
- delete：删记录 → 删文件 → 如果删的是当前版本，把**本线**最新的一条顶上来
  （`ORDER BY id DESC LIMIT 1`）。所以删掉最新版等于"回退到上一个版本"，
  但列表里那条记录还在（可以再切回来，前提是文件还在 —— 注意删记录会删文件，
  所以"再切回来"只对没删过的记录成立）。

### 4.5 鉴权

```
登录：constant_time_eq(env ADMIN_TOKEN, 表单 token)
  → 对：nonce(32 位随机) + "." + HMAC-SHA256(nonce, secret)
  → Set-Cookie: bks_admin=...; HttpOnly; SameSite=Lax; Path=/; Max-Age=7d
校验：从 Cookie 头里挑出 bks_admin，拆 nonce/sig，重算 HMAC 常数时间比较
```

- 不引 JWT、不建 session 表：一个管理员、一台机器，"签名随机串"就够，且无服务端状态
  —— 重启服务不影响已登录会话（只要 `secret.key` 还在）。
- `secret.key` 在 `DATA_DIR` 里，首次启动随机生成；删了 = 所有人重新登录。
- `SameSite=Lax` 而不是 `Strict`：管理员从别处点链接跳过来时还能保持登录。
- 口令比较是常数时间（`constant_time_eq`），避免按响应时间猜口令。

## 4.6 页面上的时间

库里存 Unix 秒（UTC）。scratch 镜像里没有 tzdata，`OffsetDateTime` 的"本地时间"
永远是 UTC，直接显示会差 8 小时（刚发的版本看着像早上 7 点发的）。
所以管理页显示时间时自己加偏移：`TZ_OFFSET`（分钟，默认 480 = 北京时间），
见 `admin.rs::short_time`。清单里的 `published_at` 仍然输出标准 RFC3339 UTC，不动。

### 4.7 表单：为什么是"服务端不信任客户端"

模板在客户端编辑，所以服务端收到的每一份 schema 都可能是旧的、坏的、或者被人手改过的。
四条防线：

1. `validate_schema()`：字段数 ≤30、key 唯一且只含 `[a-z0-9_]`、类型在 5 种之内、
   `select` 必须给 ≥2 个选项、label 非空。不通过返回 **422**，直接显示在客户端编辑器里。
2. `validate_data()`：提交时按 `template_version` 取回**当时的**定义再验一遍。
   必填、数字、日期格式、下拉值必须在选项里，逐字段返回错误。
3. `base_version` 乐观锁：保存时带上自己看到的版本号，对不上返回 **409 且不改库**。
   两人同时改同一个模板，后一个不会静默覆盖前一个。
4. `edit_token`：改模板要先用管理口令换一次（7 天）。口令只在服务端比对，
   **不落盘、不写进 exe**。没有它，任何人拿到客户端就能改所有人的模板。

### 4.8 导出 xlsx 的两条规矩

1. **一次只导一个模板。** 跨模板并排导，列会稀疏得没法看 —— 所以没选模板直接
   重定向回页面并提示。
2. **列 = 该批提交所有版本快照的并集**（按首次出现顺序）+ 提交人/填写时间/接收时间。
   同一模板改过版时，老记录没有的新列留空，**绝不能按位置填**（那是串列的根源）。

数字列整列都是数字时写成数字单元格（Excel 里能直接求和）；文本一律左对齐；
sheet 名过 31 字符要截断，并清掉 `[]:*?/\` 这些 xlsx 禁用的字符。
依赖 `rust_xlsxwriter`（纯 Rust，能静态编进 musl，镜像实测从 3.66MB 涨到 **4.92MB**）。

### 4.9 管理页是标签页（v0.3 起）

`/admin` 一次渲染全部内容，切标签由页面里几十行内联 JS 切 `hidden`：

```js
// 标签本身是 <a href="/admin?tab=x">，没有 JS 时点它照样整页跳转
// 有 JS 时 preventDefault + 切 hidden + history.replaceState 同步 URL
```

为什么不做成"点标签发请求拿一段"：内容量很小（提交记录最多 50 条），
一次渲染完省掉来回；而且筛选本来就要走 URL（`GET /admin?tab=register&template=1`），
URL 已经是状态的唯一来源，没必要再来一份。

三个细节：

- **`.pane[hidden]{display:none!important}`** —— 不加 `!important`，
  任何 `display:flex` 之类的规则都会让 `hidden` 失效，几个标签会叠在一起。
- **未知 `tab=` 回落到 `overview`**，不报错。
- **每个 POST 操作重定向时都带 `tab=`**（加人回 `users`、停用模板回 `templates`、
  导出出错回 `register`），否则一操作就跳回概览，还得再点一次。

v0.4 改成了**两级标签**，见 §4.11。

### 4.10 "谁在连服务器"

`activity` 表记录每一次有意义的行为：`check`（检查更新）/ `login` / `submit`。
字段是 `kind / who / ip / client_id / detail / at`。

- **IP**：`peer_ip()` 优先读 `X-Forwarded-For` 第一个（Lucky 反代后面的真实 IP），
  没有再用 `ConnectInfo<SocketAddr>` —— 所以 `serve` 必须换成
  `into_make_service_with_connect_info::<SocketAddr>()`，否则拿不到。
- **聚合**：按 `(ip, client_id)` 分组取 `MAX(at)`。SQLite 保证 `MAX()` 那一行对应的
  其它列跟着取那一行的值，所以不用窗口函数也能拿到"最后一次动作是什么"。
- **在线判定**：`now - at < 15 分钟`。**没有心跳**，这只是"最近有请求"，
  页面上一律如实显示"N 分钟前"，不假装在实时在线。
- **只留 7 天**，每次写入顺手清理（这张表只服务于"现在谁在连"，留历史没意义）。

### 4.11 管理页两级标签（v0.4）

顶层从五个（概览/版本/登记/模板/名单）收成四个，登记和名单各自下面再分二级：

```
概览
表单登记 ├ 表单预览   ← 看表、预览编辑、导出 xlsx
        └ 模板管理   ← 建模板、停用、删除
版本更新
登录管理 ├ 客户端     ← 客服账号白名单（姓名账号 / 密码哈希 / 启停）
        └ 管理端     ← 新增：管理员账号 + 口令
```

结构上是**两级独立的容器**，不是嵌套的 tab：

```html
<section class="pane" data-pane="register" hidden>
  <nav class="subtabs"><a class="subtab" data-sub="preview" href="?tab=register&sub=preview">表单预览</a></nav>
  <div class="subpane" data-spane="preview" hidden>…</div>
  <div class="subpane" data-spane="templates" hidden>…</div>
</section>
```

要点：

- **URL 同时带两个参数**：`?tab=register&sub=preview`。二级标签没写 `sub` 时
  走 `default_sub(tab)`（登记 → 预览，登录 → 客户端）。
- **两级都靠 `hidden` 属性**，JS 里 `showTop()` / `showSub()` 各自管一摊，
  切二级不动顶层、切顶层不动二级。
- **二级标签也用 `<a href>`**：无 JS 时点它照样整页跳转，整页可寻址。
- **重定向必须带 `sub`**：改完模板回 `?tab=register&sub=templates`，
  改完口令回 `?tab=login&sub=admins`。只带 `tab` 会被丢回二级默认页，
  操作完找不到自己刚改的那一栏。
- **未知 `sub=` 回落到该顶层的默认二级**，不报错、不 400。

### 4.12 模板软删除（v0.4）

需求是"删除模板不影响已经生成的表格"。硬删做不到 —— `form_submissions.template_id`
指向模板行，删了历史记录就查不出模板名，导出也没法取到列定义。

所以用**软删除**：

```sql
UPDATE form_templates
   SET deleted_at = ?, is_active = 0, updated_at = ?,
       name = substr(name, 1, 24) || '（已删除）'
 WHERE id = ? AND deleted_at = 0;
```

四个动作缺一不可：

| 动作 | 目的 |
|---|---|
| `deleted_at = now()` | 标记删除，所有正常查询都带 `deleted_at = 0` 过滤 |
| `is_active = 0` | 客户端拉清单时不再下发这个模板 |
| 改名加「（已删除）」后缀 | **腾出 `name` 的 UNIQUE 约束**，让"删了重做"能用回原来那个名字 |
| 保留行本身 | `form_submissions.template_id` 照样能解析出模板名，导出取得到列定义 |

配套的三条规则：

- **历史记录认名字走 `form_templates_all()`**（含已删除的），不走 `form_templates()`。
  少了这一步，已删模板的历史记录就显示成空白。
- **提交时拦已删模板**：`form_template_deleted(conn, id)` 为真时拒绝新登记，
  报"这个表单已经被删除了，不能再提交"。
- **导出照常**：导出走每条提交自带的 `template_snapshot`，跟模板在不在完全无关。
  已删模板的历史记录照样能导出。

### 4.13 管理端人员与口令（v0.4）

管理端账号存在 `admin_users` 表（见 §3.2），口令**只存 sha256 哈希**，明文不落库。

登录（`admin.rs::login`）两个入口都认：

```rust
// 1) 环境变量 ADMIN_TOKEN = 超级管理员，内部 id = 0
// 2) admin_users 表里查 sha256(given) 命中且 enabled = 1
```

关键设计：

- **口令最小长度 6 位**（`MIN_TOKEN_LEN`），太短直接拒。改口令时校验
  两次输入是否一致，不一致也拒 —— 两个输入框存在的意义就是防手滑。
- **改口令走独立路由**（`/admin/admins/token`），跟改其它字段分开，
  免得编辑别的地方时把口令清空。
- **Cookie 带身份**：`bks_admin = a{id}.{nonce}.{sig}`，HMAC-SHA256 签名。
  `auth::verify()` 返回 `Option<i64>`，id 就是登录的管理员。
  ⚠️ **v0.4 改了 Cookie 格式**（老的是 `{nonce}.{sig}`），部署后**旧 Cookie 全部失效**，
  管理员需要重新登录一次。这是有意的：老 Cookie 不带身份，没法判断是谁改的。
- **超级管理员（id = 0）不在 `admin_users` 表里**，UI 上单独一行只读展示
  （`admin_env_row`），不能停用也不能删除 —— 否则会把自己锁在门外。

### 4.14 登记编辑与导出（v0.4）

**编辑登记**：点「预览」弹出 `.sheet` 遮罩层，里面按该条记录的
`template_snapshot` 渲染成**可编辑表单**（不是只读表格）。

- **只改 `data_json`，不动快照**。改了列定义就跟同模板其它行串列了。
- 保存前用 `validate_data` **再校验一遍**，必填漏填会被挡：
  报"没存上：必填（登记人）；必填（订单号）"。
- 有 JS 时走 `fetch('/admin/forms/submission/{id}/edit')` 取 HTML 片段填进弹层；
  **无 JS 时走服务端整页渲染**（`#rec-editor` 内联）。两条路都得有，
  否则禁用 JS 就完全没法编辑。

**导出 xlsx**：按模板分组，每个分组自带一个「导出这个模板」按钮。

- **工具条上的导出按钮，没选模板时不给链接**，只显示一行提示文字。
  原因见 §10 踩坑表："导出无反应"其实是整页 400，点链接跳回来看到的
  是页面顶部的一条报错，离按钮很远，人只会觉得"点了没反应"。
- 每个分组那个「导出这个模板」按钮**永远可用**（它自带模板参数）。
- 导出的筛选参数由 `export_url(q, template)` 统一拼，
  免得漏掉 `from` / `to` 导致导出的行数跟页面上看到的不一致。

### 4.15 网页版模板编辑器（v0.5）

「表单预览」里每个模板的**【编辑】**按钮：改的是**表格结构**（有哪些列、每列什么类型），
不是表格里的数据。

**为什么要做**：客户端里本来就有模板编辑器，但客户端是**装在客服电脑上**的。
客服在班车上、领导在客户现场想加一列，客户端编辑器不一定在手边。服务端这个编辑器是
**应急/微调通道**，不是主编辑器。

**关键约束：浏览器表单不能收数组。**
`serde_urlencoded` 遇到同名参数只保留**最后一个**，所以不能发
`key[]=a&key[]=b`。因此每个字段的每个属性用**独立名字**：

```
f0_pos=1   f0_key=order_no   f0_label=订单号   f0_type=text
f0_required=1   f0_options=少发,破损   f0_del=0
f1_pos=2   f1_key=reason     ...
```

- 行号 `i` 是**表单里的下标**（0 起），不是最终位置；最终位置看 `f{i}_pos`。
- 服务端 `parse_template_form()` 按 `f{i}_{属性}` 把散落的键收成「行号 → 属性表」，
  再按 `pos` 排序 —— 排序的权威是 `pos`，`i` 只是收集时的临时下标。
- **勾了「删」的行直接跳过**（`f{i}_del=1`）；**`key` 为空的行也跳过**（空行模板）。
  这两条是"加一行"和"删一行"的实现方式：不真删 DOM，标记后跳过即可。
- `options` 用 `，` `,` `、` 三种分隔符切（客服手打习惯不统一）。
  **只有 `type == "select"` 才带 options**，其它类型的 options 一律丢掉。
- 保存走 `template_save_web()`，带 `base_version` 乐观锁：模板被别人改过就返回 409，
  页面提示重试，**不改库**。

**有 JS / 无 JS 双路径**（和 §4.14 登记编辑同一个规矩）：

| | 有 JS | 无 JS |
|---|---|---|
| 取内容 | `fetch('/admin/forms/template/{id}/edit')` 取 HTML 片段塞进弹层 | `?tpl_edit={id}` 服务端整页内联渲染 |
| 加行 | 克隆 `<template id="tpl-blank">` | 点「加一个字段」整页跳转，带 `&add=1` |
| 排序 | ↑↓ 按钮交换 `pos` | 「位置」数字框手填 |
| 删除 | 勾「删」把行收起 | 勾「删」（同） |

两条路径共用 `template_editor_html()`，避免两套渲染逻辑走偏。

**字段类型 6 种**（`kind_label()` 给中文标签）：
`text`→单行文本、`textarea`→多行文本、`number`→数字、`select`→下拉、`date`→日期、`name`→姓名。

**为什么不引入开源 Office**（这个想法被明确否掉了，理由记在这里免得反复讨论）：

- **体积与部署面**：OnlyOffice Document Server 镜像 1–2 GB，还要另起 Postgres + Redis；
  LibreOffice headless 也在几百 MB。NAS 上再压一套办公套件 = 多一个部署面、多一组漏洞面。
- **功能对不上**：这里的模板是**结构化表单**，只有 6 种字段类型。
  用办公套件去编辑一张几列的表单，是"为了拧一颗螺丝先装一整个车间"。
- **数据编辑这条路已经有解**：登记内容修改走 `/admin/forms/submission/{id}/edit` 弹层
  （按快照渲染成可编辑表单），不需要 Excel 引擎。
- **真要做"像 Excel 那样改数据"**，更合适的下一步是**纯前端 SheetJS**（约 1 MB，
  不落服务端、不新增部署面），但那解决的是改数据，不是改结构。
- **结论：暂不引入**。需要 Office 级体验就往客户端做 —— 客户端的编辑器更完备
  （有字段校验、有版本），服务端只保留应急开关和结构微调。

### 4.16 上传记录独立页（v0.5）

「表单预览」里每个模板的**【上传记录】**按钮：新窗口（`target="_blank"`）打开
`/admin/forms/template/{id}/records`，**独立页面**而不是弹层。

- **只显示最近 200 条**（`RECORDS_WINDOW = 200`）。超了副标题提示
  「只看最近 200 条，要看全部请导出 xlsx」。
  为什么不是全量：有人填了上万条时，全量渲染会把浏览器拖死，而这条记录
  本身是给人**抽查一眼**用的 —— 要全量数据请走导出。
- 走独立页面而不是弹层：独立页能直接右键复制链接发给同事、能单独开标签页对比。
- 页面骨架是 `html::RECORDS_PAGE`，复用 `html::CSS`（v0.5 把样式从 `PAGE` 里抽出来了，
  两个页面共用一套，不复制粘贴）。

### 4.17 模板永久删除（v0.5）

「模板管理 → 已删除的模板」里每个软删模板多一个**【永久删除】**按钮，
点之前有 confirm 二次确认。

`db::form_template_purge()` 在**一个事务**里做三件事：

```sql
-- 1. 先把模板名冗余进历史登记（顺序不能反：先删行就拿不到名字了）
UPDATE form_submissions SET template_name = ? WHERE template_id = ? AND (template_name IS NULL OR template_name = '');
-- 2. 删掉这个模板的字段定义历史版本
DELETE FROM form_template_versions WHERE template_id = ?;
-- 3. 删掉模板行本身
DELETE FROM form_templates WHERE id = ?;
```

返回三态 `PurgeResult`：

| 返回 | 含义 |
|---|---|
| `Purged { submissions }` | 删掉了，`submissions` 是这个模板名下的历史登记条数 |
| `NotFound` | 没有这个模板（id 不存在） |
| `NotDeleted` | **还没软删过** —— 必须先走「删除」 |

- **`NotDeleted` 是故意的闸门**：没删过的模板直接调永久删除会抹掉一张还在用的表。
  要求先软删除，等于强迫操作者先看一眼"这个表有人填过吗"。
- **历史登记一条都不删**。删模板是"不想要这张表了"，不是"连填过的记录一起抹掉"。
- 冗余名只补**这一列为空**的那些行（`(template_name IS NULL OR template_name = '')`）——
  老登记本来就靠 `form_templates.name` 取名字，不用动。
- `form_template_versions` 要一起删：那是这个模板的字段定义历史，模板都没了，
  留着只是垃圾，还会让人误以为能回滚到一个已经不存在模板的版本。

### 4.18 NAS 上已经有 OnlyOffice 了（2026-10-29 侦察结论，暂未接入）

> 这一节是**侦察记录**，不是实现。结论是"随时可以接"，但**还没接**，
> 也没有改任何代码 —— 用户决定先存档，需要时再动手。

**背景**：v0.5.0 决定"不引入开源 office"（理由见 §4.15）。这个结论是**基于当时
NAS 上只有文本编辑器**得出的。2026-10-29 用户在绿联应用中心新装了
**「在线文档」**（`com.ugreen.office` v1.20.1.0028），**前提变了，结论要重估**。

**实测事实**（全部在 NAS 上跑命令验证过，不是看文档猜的）：

| 项目 | 结果 |
|---|---|
| 应用 | `com.ugreen.office`，内含 `onlyoffice/documentserver/`（272MB 安装包） |
| 是不是 Docker | **不是**，绿联原生应用（`isDockerApp: false`） |
| 进程 | `DocService/docservice` + `FileConverter/converter` + **专用 PostgreSQL**（`/usr/ugreen/local/com.ugreen.office/psql/db`）+ redismq 队列 |
| API 存活 | `GET /ugoffice/healthcheck` → **200 `true`** |
| 浏览器可达 | Lucky 把 `/ugoffice/`、`/ugreen/v1/office/` **正确路由**到 UGOS 的 50312 口（443 上 307 跳转可达） |
| JWT | **开着**。`local.json` 里 `token.inbox/outbox: true`、`tokenRequiredParams: true` |
| 原生 API | **被绿联封装层挡了**：`/web-apps/apps/api/documents`、`/cooking/convert-to`、`/info/config` 全部 404（`info/config` 是 nginx 明确 `return 404`） |
| 绿联自己的接口 | `GET /ugreen/v1/office/openfile?path=…&office_token=…`，token 由 `getOfficeToken` 签发 |

**网络拓扑**（搞错了会白折腾）：

```
浏览器
  │ https://bks.beikeshi.cn:443   (Lucky，host 网络模式)
  │   ├─ /latest.json, /admin  → Lucky 规则 → 127.0.0.1:50205 → bks-updates 容器
  │   └─ /ugoffice/, /ugreen/v1/office/ → 307 → https://…:50312 (UGOS 自己的 nginx)
  │                                        └─ /ugoffice/ → unix:/tmp/ugoffice.sock → OnlyOffice
  │                                        └─ /ugreen/v1/office/ → unix:/var/ugreen/office_serv.sock
  └─ 绿联 nginx 只监听 80（HTTPS 全部由 Lucky/UGOS 接管）
```

⚠️ **Lucky 会抢路径**：`/ugreen/v1/editor/`（文本编辑器）在 54321 上实测是
**404 `Server: LuckyWeb`** —— Lucky 把它分发到了我们的 axum 服务，它不认识这个路径。
但 `/ugoffice/` 走的是另一条规则，能正确到 UGOS。**加新路径前先在 NAS 上实测一遍
它落到谁**，别想当然。

**三条接入路线**：

| | A. 直接嵌 | B. xlsx 文件往返 | C. 纯前端网格 |
|---|---|---|---|
| 做法 | 服务端调 `getOfficeToken` → 拼 `openfile` URL → iframe | 服务端导出 xlsx 到用户可写目录 → 用户在绿联在线文档里改 → 服务端读回 | SheetJS（约 1MB）打进服务端静态资源 |
| 改登记数据 | ✅ | ✅ | ✅ |
| 体验 | 真 Office | 真 Office（要跳出管理页） | 只有网格，无公式无格式 |
| 新增部署面 | 0 | 0 | 0 |
| 服务端要加的依赖 | 无 | `calamine`（只读 xlsx，约 1MB） | 无 |
| 主要坎 | ① `getOfficeToken` 要 UGOS 登录态（服务端得存凭据）② **保存回调未知** —— OnlyOffice 存盘要 POST 到 `callback` 才能写回服务端，而原生 API 被挡，绿联封装层是否提供回调**还没验证** | 跳出管理页；data 目录 root-only 要用 `/volume1/工具箱` 之类可写目录；要定"读回"机制（轮询 mtime 或用户点导入） | 没有 OnlyOffice 的公式、格式、合并单元格 |

**⚠️ 语义红线：路径 B 不能用来改模板结构。**
`template_snapshot` 是整套设计里最不能妥协的一条（历史数据永不串列）。
让用户在 xlsx 里改列再导回就会破坏快照冻结，且**历史数据串列后无法修复**。
路径 B 只适用于**改 `data_json` 的值**（改登记数据）。
要改结构，走现有字段编辑器或路径 C。

**没做的验证**（下次要做时先补上）：

1. 绿联在线文档能不能在浏览器里新建 xlsx → 存到 `/volume1/工具箱/` → 再打开编辑 → 保存
   （**这是路径 B 成不成立的前提**，无法用命令行代替，只能人工点）
2. 绿联封装层有没有提供 OnlyOffice 的 `callback`（**这是路径 A 成不成立的前提**）
3. `getOfficeToken` 到底要什么凭据、有效期多久

**读 NAS 上 root 私有文件的办法**（`local.json` 这类）：
用户在 `docker` 组里，可以起特权容器读：
```bash
docker run --rm --entrypoint sh -v /volume1/@appstore/com.ugreen.office:/o:ro ugreen/lucky:v3 \
  -c 'cat /o/onlyoffice/documentserver/server/Common/config/local.json'
```
⚠️ `--entrypoint sh` 不能省 —— `ugreen/lucky:v3` 默认入口是 LuckyWeb，命令会被忽略。
⚠️ Docker Hub 在这台机器上被拦，跑不了 `alpine` / `curlimages`，只能用**本地已有**的镜像。


### 4.19 表单预览的网格（v0.6，改登记数据）

「表单预览」从**记录列表**改成**网格**：一行 = 一条登记，一列 = 模板的一个字段，
单元格可以直接改。**改的是登记数据（`data_json`），不是字段定义。**

#### 列是怎么来的

列 = **当前模板的字段定义**（`form_template_versions.schema_json`），不是每条登记自带的
快照 —— 这样同一模板的所有行才对得齐。模板已经删了（软删/永久删）就退回调组内
第一条登记的快照。

老登记里多出来、当前模板已经没有的字段，按出现次数附在最后并标「**旧**」：
那一列**只读**（`grid_cell(..., editable=false)`）。为什么不给改：当前模板已经没有
这个字段了，改回去就跟别的行对不上；但数据一个字节都不丢，导出和上传记录里都看得到。

```rust
pub fn grid_columns(cur: Option<&Schema>, parsed: &[(Option<Schema>, Map<String, Value>)]) -> Vec<html::GridCol>
```

#### 有 JS / 无 JS 双路径（跟模板编辑器同一个套路）

服务端**只渲染一份 HTML**：单元格是带 `data-key` / `data-kind` / `data-opts` 的 `<td>`，
内容是纯文本。

- **没 JS**：就是纯文本 + 行尾「改」链接 → 整页跳 `?rec={id}` → 服务端渲染登记编辑表单
  （老机制，`/admin/forms/submission/save`）。
- **有 JS**：`bindGrid()` 把每个 `<td>` 换成 `input` / `select` / `input[type=date]` /
  `input[type=number]`，绑 `change`（= 失焦且值变了）→ `gridSaveRow()` → fetch JSON。

控件按 `data-kind` 选：`select` → `<select>`（选项来自 `data-opts`，用 `\u0001` 分隔，
避免选项值里带 `|` 被切错）、`date` → date、`number` → number，其余 → text。

`data-opts` 用 `\u0001` 而不是 `|`：选项值是用户填的，带竖杠不稀奇，被切错就静默丢选项。

#### 保存

```
POST /admin/forms/submission/grid-save   {"id":1,"data":{"field_3":"64545"}}
→ {"ok":true,"at":"10-29 15:15"}  或  {"ok":false,"msg":"必填（客户需求）"}
```

`data` 的 key 就是**字段 key**（不是 `f_key`）。没提到的字段**原样保留** ——
网格一次只交一行，不能把没提到的键清掉（尤其老版本遗留的字段）。

弹层那个 `POST /admin/forms/submission/save` 和它**共用** `apply_submission_edit()`：

```rust
pub(crate) fn apply_submission_edit(conn, id, values: &BTreeMap<String,String>) -> Result<(), String>
```

三条规矩（也是测试重点）：
1. 只改 `data_json`，**快照一个字节都不动**；
2. 按**提交那一刻的快照**校验，校验不过就什么都不改（报错带字段中文名）；
3. 以库里现有内容为底，只覆盖传进来的字段。

前端状态：行首橙点（`.dot`）= 还没存上；状态列 `✓ HH:MM` = 存好了；
卡片底部一条 `gsave` / `gerr` 提示（4 秒后自动消失）。**值没变不发请求**
（拿 `data-orig` 对比），免得白刷保存时间。

#### 几个明确不做的事

- **没有"新增一行"** —— 填新登记是客户端填表的职责，网页端只改已经填过的。
- **前三列只读**（`#` / 提交人 / 填写时间）—— 元数据不该手改。
- **网格最多 200 条**（跟上传记录的 `RECORDS_WINDOW` 对齐），超了在卡片标题旁提示。


## 5. 管理页模板

`html.rs` 里两个 `&'static str`（`LOGIN` / `ADMIN`），用 `%%KEY%%` 占位：

```rust
html::ADMIN
    .replace("%%SUBTITLE%%", &escape(&subtitle))
    .replace("%%ROWS%%", &rows)
    ...
```

为什么不用 `format!`：页面里有大量 CSS 花括号（`body{margin:0}`），
用 `format!` 得把每个 `{` 写成 `{{`，改样式时非常容易漏 —— 漏一个就直接 500。
`%%KEY%%` 不会有这个问题。

其它两条：

- **拼进 HTML 的动态内容一律过 `escape()`**（版本号、说明、配置里的地址都能被管理员输入）。
- **重定向消息要百分号编码**：`Location` 头只能是 ASCII，直接把中文塞进去浏览器会乱码。

## 6. 包类型检测

```rust
// 头 4MB + 尾 4MB 找 "Inno Setup"
let head = 前 min(len, 4MB);
let tail = 后 min(len, 4MB);
if 命中 => "installer" else "portable"
```

依据是 Inno 安装包的 `Inno Setup Setup Data (6.x.x)` 标记，落在 PE 的 overlay 区。
**实测该标记在文件约 90% 位置**，所以头尾各扫 4MB 能覆盖；只扫开头几百 KB 会把它判成 portable。

这个字段只用于管理页展示（列表里标"安装包"还是"程序"），不参与客户端逻辑。

## 7. 健康检查与子命令

scratch 镜像里**没有 shell、没有 curl、没有 wget**，所以：

- `/health` 路由给外部探活用（返回 `ok`）；
- `--health` 子命令给 docker healthcheck 用：**从 `BIND` 解析出地址，TCP 连一下自己**，
  连上 exit 0，否则 exit 1。只验证"进程还在 listen"，不走业务。
  （注意它读 `BIND`；本地手动测时要么带上 `BIND=...`，要么会去连默认的 `0.0.0.0:8080`。）
- `--version` 打印 `bks-updates <版本>`，方便 `docker run --rm image --version` 查镜像版本。

### 8.0 部署到 NAS：本地构建 + tar 传输（推荐路径）

**以后都用本地构建，不等 GitHub Actions**（用户 2026-10-29 明确要求）。
Actions 留着当备用（比如要多架构镜像时才用）。

```bash
# 1. 本机构建（国内要换镜像源）
docker build --platform linux/amd64 \
  --build-arg RUST_IMAGE=docker.m.daocloud.io/library/rust:1-alpine \
  -t bks-updates:local -t ghcr.io/wukaibo98/beikeshi-updates:latest .

# 2. 导出（镜像只有 ~5MB，压完 1.7MB）
docker save bks-updates:local | gzip -1 > /tmp/bks-updates.tar.gz

# 3. 传到 NAS
scp 不可用（SFTP 的路径基准跟 shell 不一致，会报 "No such file or directory"），
#   用 ssh + cat 最稳：
ssh -i ~/.ssh/id_ed25519_bks "吴凯波@192.168.124.3" \
  "cat > /volume1/docker/bkstools/bks-updates.tar.gz" < /tmp/bks-updates.tar.gz

# 4. NAS 上导入并重启
gunzip -c bks-updates.tar.gz | docker load
docker tag bks-updates:local ghcr.io/wukaibo98/beikeshi-updates:latest
cd /volume1/docker/bkstools && docker compose up -d
```

要点：

- **compose 的 image 名保持 `ghcr.io/...:latest`**，导入后打同样的 tag，
  `docker compose up -d` 就会用本地镜像（默认 `pull_policy: missing`，本地有就不拉）。
- **先 `docker compose stop` 再 load**，让 SQLite 正常 checkpoint（别删 wal，那个坑踩过）。
- 每次升级前备份 `data/updates.db` 和 `data/secret.key` 到 `backup/`。

## 8. 构建与镜像

`Dockerfile` 是多阶段，第一段就一个技巧值得记住：**先只拷 `Cargo.toml`/`Cargo.lock`
并编译一个空 `main`，把依赖层缓存住**，然后再拷 `src`；拷完必须 `touch src/main.rs`，
否则 cargo 认为源码没变会复用空壳（踩过，表现为"镜像里的程序什么都不做"）。

```dockerfile
ARG RUST_IMAGE=rust:1-alpine        # 基础镜像留成参数，国内换源用（见下）
FROM ${RUST_IMAGE} AS build
RUN apk add --no-cache musl-dev    # rusqlite bundled 要 C 编译器
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release
FROM scratch
COPY --from=build /src/target/release/bks-updates /bks-updates
ENTRYPOINT ["/bks-updates"]
```

alpine + musl 产出**完全静态**的二进制，才能放进 scratch。

`RUST_IMAGE` 是给国内环境留的口子：**直连 Docker Hub 会被拦**
（`load metadata for docker.io/library/rust:1-alpine` → `DeadlineExceeded`），
换镜像源即可，不用改文件：

```bash
# 本机实测：Intel Mac / Docker Desktop / 1 分 16 秒
docker build --platform linux/amd64 \
  --build-arg RUST_IMAGE=docker.m.daocloud.io/library/rust:1-alpine \
  -t bks-updates:local .

docker images bks-updates:local      # 3.66MB（其中二进制 2.35MB）
docker run -d --name bks-local -p 18081:8080 \
  -e ADMIN_TOKEN=test123 -e LAN_BASE=http://127.0.0.1:18081 \
  -v /tmp/bks-data:/data bks-updates:local
docker exec bks-local /bks-updates --health; echo $?   # 0
docker stats --no-stream bks-local                     # 内存 4.5MiB
```

容器里跑完整流程（登录 → 上传 → 读清单 → 下载）也已实测通过，见 HANDOVER §8。

镜像名由 workflow 的 `IMAGE: ${{ github.repository }}` 决定，即
`ghcr.io/wukaibo98/beikeshi-updates`。**所以仓库名必须叫 `beikeshi-updates`**，
否则 compose 里写死的镜像名对不上（这也是这个仓库沿用旧名字的原因）。

## 9. 改完代码怎么验证

日常验证是"跑二进制 + curl"（比构建镜像快得多，也不需要 docker）：

```bash
cargo build --release
rm -rf /tmp/bks-rs-test && mkdir -p /tmp/bks-rs-test
DATA_DIR=/tmp/bks-rs-test/data ADMIN_TOKEN=test123 BIND=127.0.0.1:18080 \
LAN_BASE=http://127.0.0.1:18080 PUBLIC_BASE=https://bks.example.com \
target/release/bks-updates &
```

```bash
H=http://127.0.0.1:18080

curl -s $H/health                                   # ok
curl -s -w " %{http_code}\n" $H/latest.json                 # 404 旧版客户端已停止发布
curl -s -w " %{http_code}\n" "$H/latest.json?client=python"  # 404 未知客户端：python
curl -s -w " %{http_code}\n" "$H/latest.json?client=web"     # 404 还没有发布任何版本（空库）

curl -s -o /dev/null -w "%{http_code}\n" -X POST -d token=wrong $H/admin/login   # 401
curl -s -c jar -o /dev/null -w "%{http_code}\n" -X POST -d token=test123 $H/admin/login  # 303

# 上传（包名固定 bkstools-<版本>.exe）
curl -s -b jar -o /dev/null -w "%{redirect_url}\n" -X POST \
  -F version=0.2.0 -F "notes=测试" -F file=@fake.exe $H/admin/upload

curl -s "$H/latest.json?client=web" | python3 -m json.tool    # 看 size/sha256/两个 url
curl -s "$H/latest.json"            # 仍是 404：旧客户端拿不到新版清单
curl -s -o got.exe "$H/packages/bkstools-0.2.0.exe" && shasum -a 256 got.exe fake.exe

curl -s -o /dev/null -w "%{http_code}\n" "$H/packages/..%2F..%2Fsecret.key"  # 400
curl -s -o /dev/null -w "%{http_code}\n" "$H/packages/secret.key"            # 404

curl -s -b jar $H/admin | grep -c 'name="client"'   # 0：客户端下拉已经删掉
curl -s -b jar $H/admin | grep -c "当前"            # 1：只有一条线，只有一个"当前"
```

清单（2026-10-28 实测全部通过）：

- [x] `/health` → 200 `ok`
- [x] 未发版 `/latest.json?client=web` → 404 + `error` 字段
- [x] 不带参数 / `client=python` → 404（旧客户端拿不到新版清单，不会误替换自己）
- [x] 错口令 → 401；对口令 → 303 + `bks_admin` Cookie
- [x] 上传（不带 client 字段）→ 303 `已发布 v0.2.0（0.03 MB，程序）`
- [x] 清单字段完整；`lan_url` / `public_url` 按 `LAN_BASE` / `PUBLIC_BASE` 拼对
- [x] 下载内容 sha256 与上传文件一致（`sort -u` 后只剩一个 hash）
- [x] 路径穿越：编码的 `../` → 400；白名单内但不存在的名字 → 404
- [x] 管理页没有客户端下拉、没有「两条版本线」提示，表头是 版本/大小/下载/发布时间/操作
- [x] 手工往库里插一条 `client='python', is_current=1` 的老记录：清单仍返回 0.2.0；
      管理页给它打「旧版·已停发」且不显示"当前"、没有"发布"按钮；
      `POST /admin/publish/<它>` → `v1.1.2 是已停发的旧版客户端记录，不能切回`
- [x] `delete` 当前版本后，本线最新一条自动顶上，被删的文件从磁盘消失
- [x] 管理页时间按 `TZ_OFFSET`（默认 +480）显示，15:20 的版本不会再显示成 07:20
- [x] `--version` 输出 `bks-updates 0.2.0`；`BIND=... --health` 退出码 0
- [x] `cargo build --release` 0 warning；`docker build` 出的镜像在容器里复验一遍同样通过

### 9.1 表单的 curl 冒烟

```bash
curl -s -c jar -o /dev/null -X POST -d token=<ADMIN_TOKEN> $H/admin/login

# 名单
curl -s -b jar -o /dev/null -X POST -d "name=张三" $H/admin/users/save
curl -s -X POST -H "content-type: application/json" -d '{"name":"查无此人"}' $H/form/login
#   → 403 {"error":"姓名不在名单里，请联系管理员"}
TOK=$(curl -s -X POST -H "content-type: application/json" -d '{"name":"张三"}' $H/form/login \
      | python3 -c "import json,sys; print(json.load(sys.stdin)['token'])")
ET=$(curl -s -X POST -H "content-type: application/json" -d '{"token":"<ADMIN_TOKEN>"}' \
     $H/form/edit-login | python3 -c "import json,sys; print(json.load(sys.stdin)['edit_token'])")

# 模板：无 edit_token → 403；坏 schema → 422；同 base_version 存两次 → 第二次 409
curl -s -X POST -H "content-type: application/json" -H "x-bks-edit: $ET" \
  -d '{"name":"补发申请","schema":{"fields":[{"key":"a","label":"A","type":"text","required":true}]}}' \
  $H/form/template/save                       # {"id":1,"version":1}

# 提交：created → duplicate → 必填缺失 422 → 下拉值非法 422
SID=aaaaaaaa-1111-2222-3333-444444444444
curl -s -X POST -H "content-type: application/json" -H "x-bks-form: $TOK" \
  -d "{\"submission_id\":\"$SID\",\"template_id\":1,\"template_version\":1,\"data\":{\"a\":\"1\"}}" \
  $H/form/submit                              # {"status":"created"}
# 再发一次同一个 SID → {"status":"duplicate"}，库里仍只有一条

# 导出
curl -s -b jar -D - -o out.xlsx "$H/admin/forms/export.xlsx?template=1"
python3 -c "import zipfile;print(zipfile.ZipFile('out.xlsx').namelist()[:3])"
```

2026-10-28 实测 **36 项全过**（登录/名单/口令/模板 CRUD/乐观锁/提交幂等/校验/
旧版本补交/停用开关/日期筛选/导出内容与列顺序/老库迁移）。

v0.3.0 标签页改版后又跑了一遍：五个 `?tab=` 各只有一段可见、徽章计数对、
概览活动行带 IP 与动作、删除用户后提交保留、导出重定向带 `tab=`、
`/admin/forms` 302、`?tab=hack` 回落概览；本机 + NAS 各一次。

### 9.2 单元测试

```bash
~/.cargo/bin/cargo test --release     # 32 项
```

测的都是"错了也不报错"的地方：重复提交只落一条、模板升版后旧快照仍在、
乐观锁被拒时库没被改、名单外登录失败、坏 schema 存不进、导出列并集缺列留空、
删模板后历史记录和名字都还在、改登记只动内容不动字段定义、
管理端口令只存哈希、停用/改口令后老口令登不进来、查询参数空串当没填。

v0.5 新增 6 项：

| 测试 | 测什么 |
|---|---|
| `网页版编辑器_解析出字段并按位置排序` | `f{i}_{属性}` 能收成字段，顺序按 `pos` 不按下标 |
| `网页版编辑器_删掉的行和空行都不算` | 勾「删」的行、`key` 为空的行都不进结果 |
| `提交时把模板名冗余存进登记` | `form_submissions.template_name` 有值 |
| `永久删除后模板行没了但登记和名字都还在` | 模板行真删，登记一条不少，名字认得出 |
| `没删过的模板不能永久删除` | 返回 `NotDeleted`（闸门有效） |
| `永久删除会清掉字段定义的历史版本` | `form_template_versions` 里该模板的行被清掉 |

v0.6 新增 4 项（都在测 `apply_submission_edit`，网格和弹层共用它）：

| 测试 | 测什么 |
|---|---|
| `网格的列_当前模板字段在前_旧字段附在后面并标旧` | 列按「当前模板字段 + 旧字段」排，旧字段带 `legacy`，下拉列带选项 |
| `网格改登记_只改传进来的字段_没提到的和旧字段都原样保留` | 只覆盖传进来的；没提到的键、老字段都不被清空；快照不动 |
| `网格改登记_清空必填会被拒_而且库里的东西一点没改` | 校验不过就 `Err`，报错带中文名，库里原样 |
| `网格改登记_改一条不影响同模板的其它行` | 改 id=1 不动 id=2 |

**改 `forms.rs` / `db.rs` / `xlsx.rs` 之后必须跑**，这些是静默错误，界面上看不出来。

### 9.3 无头 Chrome 截管理页（v0.4 起）

管理页改版后光看 curl 返回的 HTML 看不出对不对（两级标签、弹层、遮罩层
这类交互问题，HTML 静态看是"都在"的）。用 `tools/shot-admin.js` 截图：

```bash
node tools/shot-admin.js http://127.0.0.1:18080 /tmp/admin.png
```

- 走 **Node 22 内置 `WebSocket`** 直接连 CDP，不装 puppeteer（省依赖）。
- **必须用 `Network.setCookie` 注入自签 Cookie**，不能用登录表单。
  原因是 `file://` → `http://` 属跨站，Chrome 的第三方 cookie 策略会把
  `Set-Cookie` 拦掉，截出来还是登录页。
- Cookie 格式是 v0.4 的 `a{id}.{nonce}.{sig}`，签名密钥是 `DATA_DIR/secret.key`。

**第 7 个参数是「截图前先点一下」**（v0.5 加）——弹层、编辑器这类**要点击才出现**的东西，
静态截图拍不到，得先模拟点击：

```bash
# 1. 预览页三个按钮
node tools/shot-admin.js http://127.0.0.1:18080 /tmp/1-preview.png
# 2. 点开模板编辑器弹层
node tools/shot-admin.js http://127.0.0.1:18080 /tmp/2-editor.png '.tpl-open'
# 3. 上传记录独立页（直接开 URL）
node tools/shot-admin.js http://127.0.0.1:18080/admin/forms/template/1/records /tmp/3-records.png
```

点击是用 `Runtime.evaluate` 执行 `document.querySelector(sel).click()`，
找不到元素会返回 `NOT_FOUND` —— **参数写错时不会报错，只会截到没弹层的页面**，
所以截图后要肉眼看一眼弹层在不在。点击后等 1.8 秒再截（等 fetch 回来填弹层）。

⚠️ **`baseUrl` 要带到 `/admin`**：只写 `http://127.0.0.1:18080` 会命中 `/` 的 307 重定向，
截出来是「概览」标签，看不出问题在哪（v0.6 踩过）。

### 9.4 网格交互的 CDP 实测（v0.6 起）

截图只能证明"页面长这样"，证明不了"改了真的存进去了"。用 `tools/grid-cdp-test.js`：

```bash
# 本地起服务 + 造几条登记后：
node tools/grid-cdp-test.js <DATA_DIR/secret.key> http://127.0.0.1:18080
```

它做 12 项检查：控件是否按字段类型建出来（text/date/select/number 各自的数量）、
下拉选中值是不是当前值、有没有残留没换掉的文本格、改值后**立刻**亮未存标记、
保存成功后标记消失且状态列显示时间、底部出现已保存提示、
**值没变不重复保存**、清空必填被拒且提示带中文名、下拉改动能存上、
「改」链接打开弹层。退出码非 0 就是有 FAIL。

写这个脚本时踩到的两个自己的坑：
- **别把测试值写死**。上一轮跑完会把测试值留在库里，下一轮 `data-orig` 正好等于它，
  `change` 就被"值没变"的分支吃掉，看起来像功能坏了。先读 `data-orig` 再生成不同的值。
- **`dispatchEvent(new Event('change'))` 才能模拟失焦保存**。直接 `el.value = x` 不派发任何事件。


## 10. 踩过的坑（改这里前必读）

| 坑 | 说明 |
|---|---|
| 404 ≠ 故障 | 没发版时 `/latest.json` 返回 404 是给客户端**语义化**用的。并进通用异常后，客户端会报"内外网都连不上"，排查方向全错 |
| `format!` 渲染 HTML | 页面里满是 CSS 花括号，会当占位符解析 → 500。统一 `%%占位%%` + `replace` |
| `is_current` 更新漏 client 条件 | 库里还有旧版记录，不加条件会把它们的历史标记也清掉，回滚时才发现 |
| 导出按位置填列而不是按 key | 模板一改版就串列，且历史数据无法修复 —— 必须按快照取列 |
| 提交不存快照 | 同上。快照是这套设计里最不能妥协的一条 |
| 忘了 axum 默认 body 限制是 2MB | 大包传一半就被截断，浏览器报"连接被重置"，看起来像网络问题 |
| 上传出错不删临时文件 | `packages/` 里堆满 `.upload-*.tmp` |
| 把 `duplicate` 当错误返回 | 客户端只有看到成功才清队列，一直重试会让"待上传"永远清不掉 |
| 允许保存离线编辑的模板 | 模板是全局配置，离线改会分叉成两套 —— 断网时保存按钮应置灰 |
| `.pane[hidden]` 不加 `!important` | 被别的 display 规则盖掉，五个标签会同时显示 |
| 忘了 `into_make_service_with_connect_info` | `ConnectInfo<SocketAddr>` 提取失败，运行期直接 500 |
| 只信 `ConnectInfo` 的 IP | 反代后面拿到的是 Lucky 的地址，全都一样，概览页等于没用 |
| 删人时把历史提交一起删 | 记录里冗余存了 `user_name`，只需删人；连提交一起删等于毁账 |
| 把清单发给旧客户端 | 旧 Python 客户端没有 `client` 校验，会拿新版 exe 把自己替换掉 —— 所以缺参数一律 404 |
| `ALTER TABLE` 那段别删 | NAS 上的库是旧版建的，靠它补列 |
| 查询参数空串把整页打成 400 | 下拉框"全部"选项的值是空串，`?template=` → serde 报 `cannot parse integer from empty string` → **整页 400**。必须用 `opt_i64` 自定义反序列化，空串和乱填一律当 `None` |
| 软删除被 `name UNIQUE` 卡住 | 删掉"客户需求表"后同名新建报"已经有同名模板了"。删除时把名字改成 `原名（已删除）` 腾出约束 |
| JS 找 `#rec-editor` 找不到 | 服务端渲染时没包这层外壳 → 弹层永远打不开，编辑器一直内联在页面上 |
| 报错离按钮太远 = "点了没反应" | 导出失败跳回页面顶部的报错条，用户视线还在底部的按钮上。**无条件的操作不给链接，改成提示文字** |
| Location 头塞中文 | 只能 ASCII；跳转消息一律百分号编码（`admin.rs::encode`） |
| 上传失败不删文件 | `packages/` 会攒孤儿文件，且 `/packages/{name}` 还能下到它们（虽然清单里没有） |
| 目录穿越 | 下载只接受字符白名单（字母数字 + `._-`），从字符层杜绝，不靠事后规范化 |
| healthcheck 用 `/health` | scratch 里没有 curl，只能用 `--health` 子命令；它读 `BIND`，容器里必须带上 |
| 依赖层缓存 | Dockerfile 里 `touch src/main.rs` 不能少，否则编出来的镜像是空壳 |
| 仓库名 | workflow 用 `github.repository` 当镜像名，仓库必须叫 `beikeshi-updates` |
| 端口 | 容器内 8080，NAS 映射 50205；公网走 Lucky 反代到 `:54321`。改这里要同时改客户端里的两个地址常量 |
| 模板永久删除前不先冗余名字 | 模板行一删，历史登记就变成认不出表名的孤儿。先 `UPDATE ... SET template_name` 再 `DELETE`，**顺序不能反** |
| `form_submissions.template_name` 加列 | 老库要 `ALTER TABLE ... ADD COLUMN template_name TEXT NOT NULL DEFAULT ''`；**别删**，NAS 上那份库是 v0.4 建的 |
| 浏览器表单收不了数组 | `serde_urlencoded` 遇到同名参数只保留最后一个，`key[]=a&key[]=b` 会丢。字段属性必须用独立名字 `f{i}_{属性}` |
| `f{i}` 下标 ≠ 最终位置 | 下标只是收集时的临时下标（删一行后下标会跳）。排序的权威是 `f{i}_pos`，按 `pos` 排 |
| 抄 CSS 时抓到登录页的 | `html.rs` 里有多个 `<style>`，用 `re.search` 会抓到第一个（LOGIN 的 14 行）。要按 `pub const PAGE` 的位置定位再往后找 |
| GitHub API 归一化提交日期 | API 存的是 UTC。git author 行里**时间戳是绝对纪元秒**，`+0800` 只是显示标签 —— 归一化是**只改标签不动时间戳**，减 8 小时会变成另一个时刻 |
| GitHub API 不补 message 尾换行 | 本地 `git commit` 总带尾 `\n`，API 不会补。要 SHA 完全一致只能用 `git hash-object -t commit -w --stdin` 精确写字节，`git commit --amend` 做不到 |
| `git diff-tree -z` 的输出格式 | `-z` 模式下是 `"<header>\0<path>\0"` **成对交替**，不是 tab 分隔。按 tab 切会切错行 |
| 网格的 `data-opts` 用 `\|` 分隔 | 选项值是用户填的，带竖杠不稀奇，被切错就静默丢选项。改用 `\u0001` |
| 网格测试把值写死 | 上一轮跑完测试值留在库里，下一轮 `data-orig` 正好等于它 → `change` 被"值没变"分支吃掉，看着像功能坏了。先读原值再生成不同的值 |
| 截图 `baseUrl` 只写到端口 | 命中 `/` 的 307 → 截到「概览」标签。要写到 `/admin` |
| `docker build` 报 `DeadlineExceeded` | 国内直连 Docker Hub 超时。换源：`docker build --build-arg RUST_IMAGE=docker.m.daocloud.io/library/rust:1-alpine .` |
| 备份只拷 `updates.db` | 停容器后 db 只有 4096 字节，真实数据在 `-wal` 里。必须四个文件一起：`updates.db` + `-wal` + `-shm` + `secret.key` |
| `docker cp` 到不存在的容器 | `docker cp <容器>:<路径> <另一个容器>:<路径>` 会静默失败。正确做法：先 `docker cp` 到宿主 `/tmp`，再挪进备份目录（`/tmp` 是 root 写的，SSH 用户能读） |

## 11. 与客户端的关系

客户端（`../beikeshi-cs-web`，仓库 `wukaibo98/beikeshi-cs-web`）里有两处硬编码地址：

```csharp
"https://bks.beikeshi.cn:54321/latest.json",
"http://192.168.124.3:50205/latest.json",
```

外加 `?client=web` 参数（`UpdatePolicy.ExpectedClient`）。所以本服务改端口、改域名、
改客户端标识，都要同步改客户端并发新版 —— 顺序上先发服务端没问题（旧客户端还认老地址），
但反过来会断更新链路。

清单字段的消费方在客户端 `Bks.Core/ReleaseManifest.cs` 与 `UpdatePolicy.cs`，
那边有 43 项单元测试，改清单结构要两边一起动。

## 12. 数据库并发（2026-10-31，ChatGPT 修改）

- 运行期使用 `r2d2_sqlite` 连接池，池上限 8；启动时仍先用单连接执行 schema 初始化，再由池连接服务。
- 启动初始化时设置数据库 WAL 模式；每条池连接设置 `synchronous=NORMAL` 和 5 秒 `busy_timeout`。WAL 允许读请求与写请求并发；SQLite 写入仍由 SQLite 自身串行化。
- 请求处理从池中获取连接，不再争抢一把全局 `Mutex<Connection>`。
- 导出查询、导出数据转换和 xlsx 序列化都移入 Tokio `spawn_blocking`，避免重 CPU/同步磁盘工作堵住异步 worker。
- 新依赖锁定在 `Cargo.lock`；新增连接池版本选用 Rust 1.75 兼容依赖区间。
  `cargo test --release` **38 项全过**；镜像 `bks-updates:1.6.6` 已本地构建，**尚未部署**。

## 13. 客服账号密码（2026-10-31，ChatGPT 修改，未发布）

- `form_users.name` 同时作为账号和记录展示名；新增 `password_hash`（Argon2id 编码串）和 `auth_version`。
- 老库迁移给 `password_hash` 空值；管理员必须给既有账号设初始密码。新客服添加时要求填写并确认至少 10 字符密码。
- `/form/login` 校验姓名账号与密码，失败按来源 IP 每 15 分钟最多 5 次；登录流程不保存或回传密码。
- 客服可通过 `/form/password/change` 验证旧密码并改密。管理员可重置密码；改名、改密、停用或启用都会递增 `auth_version`，旧令牌随即失效。
- 登录令牌包含账号版本。客户端不能再用已存姓名自动续登；令牌过期后需重新输入密码。部署需先发新版客户端，再更新服务端，并给老账号设置初始密码。


## 14. 2026-10-31 功能修改记录（ChatGPT 修改，未部署）

- 在线状态：新增 `form_sessions` 表，使用 `(user_id, client_id)` 作为设备会话主键，心跳更新设备 IP 与时间。`POST /form/heartbeat` 要求有效客服令牌；管理员概览逐个展示登录管理名单，180 秒内有心跳为在线，可同时列出多台设备。客户端每 60 秒发心跳，账号令牌被服务端撤销时本地清除会话。老数据库启动时自动建表。
- 物流字段：schema 新增 `type: express`、`groupByCarrier`。提交值结构为 `{ "tracking_number": "...", "carrier": "..." }`，服务端会复验字段值。客户端根据字母前缀（SF、JD、YT、ZT、YD、JT、EMS 等）和少数较常见数字前缀尝试匹配，并允许直接输入/修改公司名。该规则是本地启发式，不是承运商官方验证。数字编码可能重复；快递鸟的单号识别 API 文档说明识别结果可能是一家或多家公司，并按概率排序；因此未覆盖或容易冲突的数字格式留给人工选择。参考：[快递鸟单号识别 API](https://v2.kdniao.com/api-recognise)、[常见号段说明](https://www.kdniao.com/message/detail/126818)、[开源正则规则示例](https://gist.github.com/shallowlong/7d5501b92666742df0b9c5bda03d0a17)。
- 分组导出：当前模板中至多一个物流字段勾选 `groupByCarrier` 后，导出生成单个 xlsx，各公司单独一个 sheet；缺公司值归入「未识别或未填写」。sheet 名按 Excel 限制清洗并处理重名。历史模板快照用于生成列，分组字段配置取当前模板。
- 记录页：预览中只保留模板名称、记录数和操作按钮；「上传记录」独立页显示最近 200 条可编辑网格，提交仍共用 `apply_submission_edit()` 和快照校验。
- 编译：`cargo build --release` 0 warning；`cargo test --release` **38 项全过**；功能尚未部署。

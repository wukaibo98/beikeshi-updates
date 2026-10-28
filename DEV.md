# 贝可施更新分发服务 — 开发文档

> 面向改这个服务的人。部署与日常发版见 [README.md](README.md)，交接要点与待办见 [HANDOVER.md](HANDOVER.md)。

**服务端 v0.2.0 ｜ Rust 1.75+ / axum 0.8 / rusqlite(bundled) / tokio ｜ 静态二进制 → scratch 镜像**
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
auth.rs   ── HMAC-SHA256 签名：管理 Cookie / 表单 token / 编辑 token
forms.rs  ── schema 校验、姓名登录、模板存取（乐观锁）、提交（幂等+复验）
          ├─ GET  /admin/forms            表单管理页（提交/模板/名单三块）
          └─ GET  /admin/forms/export.xlsx 导出（一次只导一个模板）
xlsx.rs   ── rust_xlsxwriter：列并集 → 数字列 → sheet 名清洗
html.rs   ── 登录页 / 分发页 / 表单页模板（`%%占位%%`）
tests.rs  ── `cargo test`：幂等、快照冻结、乐观锁、白名单、导出列
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
form_users (id, name UNIQUE, enabled, created_at)      -- 姓名白名单，登录只查"在不在"
form_templates (id, name UNIQUE, version, is_active, sort_order, updated_at)
form_template_versions (template_id, version, schema_json, created_at)  -- 每个版本的字段定义
form_submissions (id, submission_id UNIQUE, template_id, template_version,
                  template_snapshot, user_id, user_name, data_json,
                  client_time, created_at, client_id)
```

关键语义：

- **`template_snapshot` 是提交那一刻的字段定义，冻结不动。**
  模板一改，历史提交的列就变了；导出按每条提交自带的快照取列，永不串行。
  配套：`form_template_versions` 保存**每个版本**的定义，旧的也能取回来。
- **`submission_id UNIQUE` + `ON CONFLICT DO NOTHING`** 是幂等的命根子。
  客户端生成一次，所有重试用同一个；服务端返回 `duplicate` 也算成功
  （客户端只有看到成功才会清队列，网络超时其实已入库时重试不会产生第二条）。
- **`client_time` 和 `created_at` 分开**：离线补交的单子可能几小时后才到，
  报表一律按"客服填的时间"排，服务器收到的时间只用于排查。
- **`user_name` 冗余存一份**：改名/删人后历史记录还认得出是谁填的。
- 四张表都在启动时的 `init_forms()` 里 `CREATE TABLE IF NOT EXISTS`，
  NAS 上那份老库原地加表，不用迁移脚本。

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
依赖 `rust_xlsxwriter`（纯 Rust，能静态编进 musl，镜像从 3.66MB 涨到约 4.4MB）。

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

### 9.2 单元测试

```bash
~/.cargo/bin/cargo test --release     # 12 项
```

测的都是"错了也不报错"的地方：重复提交只落一条、模板升版后旧快照仍在、
乐观锁被拒时库没被改、名单外登录失败、坏 schema 存不进、导出列并集缺列留空。
**改 `forms.rs` / `db.rs` / `xlsx.rs` 之后必须跑**，这些是静默错误，界面上看不出来。

## 10. 踩过的坑（改这里前必读）

| 坑 | 说明 |
|---|---|
| 404 ≠ 故障 | 没发版时 `/latest.json` 返回 404 是给客户端**语义化**用的。并进通用异常后，客户端会报"内外网都连不上"，排查方向全错 |
| `format!` 渲染 HTML | 页面里满是 CSS 花括号，会当占位符解析 → 500。统一 `%%占位%%` + `replace` |
| `is_current` 更新漏 client 条件 | 库里还有旧版记录，不加条件会把它们的历史标记也清掉，回滚时才发现 |
| 导出按位置填列而不是按 key | 模板一改版就串列，且历史数据无法修复 —— 必须按快照取列 |
| 提交不存快照 | 同上。快照是这套设计里最不能妥协的一条 |
| 把 `duplicate` 当错误返回 | 客户端只有看到成功才清队列，一直重试会让"待上传"永远清不掉 |
| 允许保存离线编辑的模板 | 模板是全局配置，离线改会分叉成两套 —— 断网时保存按钮应置灰 |
| 把清单发给旧客户端 | 旧 Python 客户端没有 `client` 校验，会拿新版 exe 把自己替换掉 —— 所以缺参数一律 404 |
| `ALTER TABLE` 那段别删 | NAS 上的库是旧版建的，靠它补列 |
| Location 头塞中文 | 只能 ASCII；跳转消息一律百分号编码（`admin.rs::encode`） |
| 上传失败不删文件 | `packages/` 会攒孤儿文件，且 `/packages/{name}` 还能下到它们（虽然清单里没有） |
| 目录穿越 | 下载只接受字符白名单（字母数字 + `._-`），从字符层杜绝，不靠事后规范化 |
| healthcheck 用 `/health` | scratch 里没有 curl，只能用 `--health` 子命令；它读 `BIND`，容器里必须带上 |
| 依赖层缓存 | Dockerfile 里 `touch src/main.rs` 不能少，否则编出来的镜像是空壳 |
| 仓库名 | workflow 用 `github.repository` 当镜像名，仓库必须叫 `beikeshi-updates` |
| 端口 | 容器内 8080，NAS 映射 50205；公网走 Lucky 反代到 `:54321`。改这里要同时改客户端里的两个地址常量 |

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

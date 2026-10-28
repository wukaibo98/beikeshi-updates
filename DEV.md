# 贝可施更新分发服务 — 开发文档

> 面向改这个服务的人。部署与日常发版见 [README.md](README.md)，交接要点与待办见 [HANDOVER.md](HANDOVER.md)。

**服务端 v0.1.0 ｜ Rust 1.75+ / axum 0.8 / rusqlite(bundled) / tokio ｜ 静态二进制 → scratch 镜像**
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
db.rs     ── 两张表的全部 SQL；表结构与 Python 版兼容（多 client 列）
auth.rs   ── HMAC-SHA256 签名 Cookie；常数时间比较
html.rs   ── 登录页 / 管理页模板（`%%占位%%`）
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
    is_current INTEGER DEFAULT 0,         -- 1 = 该 client 线上当前对外发布的版本
    package    TEXT DEFAULT 'portable',   -- installer / portable
    client     TEXT DEFAULT 'python'      -- python / web
);

CREATE TABLE checks (                     -- 单行表：客户端检查次数的累计
    id      INTEGER PRIMARY KEY CHECK (id = 1),
    count   INTEGER DEFAULT 0,
    last_at INTEGER DEFAULT 0
);
```

几条语义要记牢：

- **`is_current` 是"每个 client 各有一行是 1"**，不是全表唯一。
  所有更新它的 SQL 都必须带 `WHERE client = ?`（`insert_release` / `publish` / `delete` 三处）。
  漏了这个条件 = 发布网页版会把 Python 版顶下线，老客服再也收不到更新。
- **`filename` 由服务端拼**，不采信上传上来的名字；前缀按 client 分流：
  `python → BeikeshiCS-<版本>.exe`，`web → bkstools-<版本>.exe`。
- **老库升级靠两行 `ALTER TABLE ... ADD COLUMN`**（`package` / `client`），
  失败就说明列已存在，用 `let _ =` 忽略。**别删**，NAS 上那份库是旧版建的。
- `journal_mode = WAL` + `synchronous = NORMAL`：读不挡写，二十个客服同时查更新不互相等；
  断电最多丢最后几条计数，版本记录本身不会坏。

## 4. 接口实现细节

### 4.1 `GET /latest.json?client=`

```
client 缺省 → "python"（旧客户端不传参数）
client 不在 ["python","web"] → 400 {"error":"未知客户端：xx"}
按 (is_current=1, client=?) 取一行；没有 → 404 {"error":"还没有发布任何版本"}
拼 windows_x64: {size, sha256, lan_url?, public_url?}
响应头一律 Cache-Control: no-store
```

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
  client  → 小写，缺省 python
  file    → 流式写到 packages/.upload-<时间戳>.tmp，边写边算 sha256
            超过 300MB 中止并删临时文件；0 字节拒绝
  → rename 到最终文件名（服务端拼）
  → detect_package() 判断 installer / portable
  → insert_release()：同一 client 的旧 is_current 清 0，新记录置 1
  → 303 重定向回 /admin?ok=<百分号编码的中文消息>
  → 任何一步失败：删掉已落盘的文件，303 到 /admin?err=...
```

**上传即发布**，没有"草稿"状态。理由是发版频率极低（一周一次都不到），
少一个状态就少一种"忘了点发布"的故障。

失败要把文件删掉，否则 `packages/` 里会慢慢攒下一堆没有数据库记录的孤儿文件。

### 4.4 `POST /admin/publish/{id}` / `delete/{id}`

- publish：先 `UPDATE ... is_current=0 WHERE client=?`，再把自己置 1；
  这就是"回滚"——不需要额外的版本序列号。
- delete：删记录 → 删文件 → 如果删的是当前版本，把**同 client** 最新的一条顶上来
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
curl -s -w " %{http_code}\n" $H/latest.json          # 404 {"error":"还没有发布任何版本"}
curl -s -w " %{http_code}\n" "$H/latest.json?client=xx"   # 400 未知客户端

curl -s -o /dev/null -w "%{http_code}\n" -X POST -d token=wrong $H/admin/login   # 401
curl -s -c jar -o /dev/null -w "%{http_code}\n" -X POST -d token=test123 $H/admin/login  # 303

# 上传（client=web 的话包名会是 bkstools-<版本>.exe）
curl -s -b jar -o /dev/null -w "%{redirect_url}\n" -X POST \
  -F client=web -F version=0.2.0 -F "notes=测试" -F file=@fake.exe $H/admin/upload

curl -s "$H/latest.json?client=web" | python3 -m json.tool    # 看 size/sha256/两个 url
curl -s "$H/latest.json"            # python 线应该还是 404（两条线互不影响）
curl -s -o got.exe "$H/packages/bkstools-0.2.0.exe" && shasum -a 256 got.exe fake.exe

curl -s -o /dev/null -w "%{http_code}\n" "$H/packages/..%2F..%2Fsecret.key"  # 400
curl -s -o /dev/null -w "%{http_code}\n" "$H/packages/secret.key"            # 404

curl -s -b jar $H/admin | grep -c "当前"        # 两条线各一个"当前"
```

清单（2026-10-28 实测全部通过）：

- [x] `/health` → 200 `ok`
- [x] 未发版 `/latest.json` → 404 + `error` 字段
- [x] 未知 client → 400
- [x] 错口令 → 401；对口令 → 303 + `bks_admin` Cookie
- [x] 上传 → 303 `已发布 网页版 v0.2.0（0.20 MB，安装包）`（假文件里放了 Inno 标记，判对了）
- [x] 清单字段完整；`lan_url` / `public_url` 按 `LAN_BASE` / `PUBLIC_BASE` 拼对
- [x] 下载内容 sha256 与上传文件一致（`sort -u` 后只剩一个 hash）
- [x] 路径穿越：编码的 `../` → 400；白名单内但不存在的名字 → 404
- [x] 上传 python 线 1.1.2 后，两条线各自返回自己的当前版本
- [x] `publish/1` 把 web 切回 0.2.0，python 线不受影响
- [x] `delete` 当前版本后，同 client 最新一条（0.3.0）自动顶上，被删的文件从磁盘消失
- [x] `--version` 输出 `bks-updates 0.1.0`；`BIND=... --health` 退出码 0
- [x] `cargo build --release` 0 warning

## 10. 踩过的坑（改这里前必读）

| 坑 | 说明 |
|---|---|
| 404 ≠ 故障 | 没发版时 `/latest.json` 返回 404 是给客户端**语义化**用的。并进通用异常后，客户端会报"内外网都连不上"，排查方向全错 |
| `format!` 渲染 HTML | 页面里满是 CSS 花括号，会当占位符解析 → 500。统一 `%%占位%%` + `replace` |
| `is_current` 更新漏 client 条件 | 发布一条线会把另一条线顶下线，客户端从此收不到更新 |
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

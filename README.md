# 贝可施更新分发服务

给「贝可施客服小工具」发版本、发安装包的小服务。**一个静态二进制 + 一个 SQLite 文件**，
没有别的依赖：NAS 上不用装运行时，镜像里只有一个 1.8MB 的可执行文件。

```bash
GET /latest.json?client=web      # 客户端拿清单
GET /packages/bkstools-0.2.0.exe # 下包
GET /admin                       # 管理页：上传 / 发布 / 回滚 / 删除
```

当前服务端版本 **0.1.0**（跟客户端版本无关）。跑在 NAS 上，镜像
`ghcr.io/wukaibo98/beikeshi-updates:latest`，容器内端口 8080，NAS 映射到 **50205**；
公网经 Lucky 反代到 `https://bks.beikeshi.cn:54321`。

## 三份文档

| 文档 | 给谁看 | 内容 |
|---|---|---|
| **README.md**（本文） | 部署 / 发版的人 | 做什么用的、接口、环境变量、怎么部署与升级、日常发版流程、备份 |
| [DEV.md](DEV.md) | 改代码的人 | 技术选型、模块地图、数据模型、每个接口的内部流程、安全取舍、构建镜像、验证清单、踩过的坑 |
| [HANDOVER.md](HANDOVER.md) | 接手的人 | 五分钟上手、环境注意事项、决策记录、待办、排查手册、实测记录 |

## 为什么用 Rust 重写

前一版是 Python（FastAPI + uvicorn，420 行），功能没有任何问题。换 Rust 的收益
**全在部署**：这台服务跑在 NAS 上，常年不动，越"没东西可坏"越好。

| | Python 版 | 现在 |
|---|---|---|
| 运行时依赖 | 基础镜像 45MB + fastapi/uvicorn 等 15MB | 无（静态二进制，镜像里只有一个文件） |
| 镜像体积 | ~60MB | **~4MB** |
| 内存占用 | 60~80MB | ~5MB |
| 启动 | 1~2 秒（import 一堆包） | <50ms |
| NAS 上要装的东西 | Python 运行时 + 依赖 | 什么都不用 |

接口、环境变量、镜像名、数据库表结构**全部保持兼容**，NAS 上原地替换即可。

## 最重要的一件事：两条版本线

客户端分两种，它们的版本**完全独立**，靠清单里的 `client` 字段区分：

```
GET /latest.json               → Python 版（旧客户端，不传参数，默认就是它）
GET /latest.json?client=web    → 网页版（C# + WebView2 客户端）
```

不分开会出大事：网页版客户端看到 Python 版的清单，会把 Python 的 exe 当成"新版本"
下载下来覆盖自己 —— 两个包都是 `.exe`，客户端光看文件辨不出来。所以：

- 服务端按 client 分别维护"当前版本"，发布网页版**不会**顶掉 Python 版；
- 客户端还会自己校验清单里的 `client` 是否等于期望值，不等就拒绝安装（双保险）；
- 包文件名带前缀：`BeikeshiCS-1.1.2.exe`（Python 版）/ `bkstools-0.2.0.exe`（网页版）。

## 接口

### 对外（客户端用，无需登录）

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/health` | 探活，返回 `ok` |
| GET | `/latest.json?client=` | 版本清单，**每次现生成**（发布/回滚后立刻生效） |
| GET | `/packages/{filename}` | 下载程序包，顺带给该版本累加下载计数 |

清单格式（客户端只认这个）：

```json
{
  "channel": "stable",
  "client": "web",
  "version": "0.2.0",
  "published_at": "2026-10-28T05:37:16Z",
  "notes": "启动画面、窗口锁高、居中、图标",
  "package": "portable",
  "windows_x64": {
    "lan_url": "http://192.168.124.3:50205/packages/bkstools-0.2.0.exe",
    "public_url": "https://bks.beikeshi.cn:54321/packages/bkstools-0.2.0.exe",
    "sha256": "7d0dde85eadefebcdbd09e185c39386a3e04ea06ab820a682a9e113dfc12ced1",
    "size": 67422649
  }
}
```

- `lan_url` 只在配了 `LAN_BASE` 时出现，`public_url` 只在配了 `PUBLIC_BASE` 时出现；
  两个都没配就退化成相对路径 `/packages/xxx.exe`。
- 客户端优先用内网地址（清单本来就是从内网拿到的话），失败再走公网。
- **没有发布过版本时返回 404 + `{"error": "还没有发布任何版本"}`**。
  这个 404 是有语义的：客户端要能区分"服务活着但没发版"和"连不上服务器"，
  别把它并进通用网络异常。

### 管理页（浏览器用，除登录外都要 Cookie）

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/admin` | 管理页；未登录时显示登录页 |
| POST | `/admin/login` | 表单 `token=...`，对就发 7 天有效的签名 Cookie；错返回 401 |
| POST | `/admin/logout` | 退出 |
| POST | `/admin/upload` | multipart：`client` / `version` / `notes` / `file`，**上传即发布** |
| POST | `/admin/publish/{id}` | 把某个历史版本切回"当前"（回滚） |
| POST | `/admin/delete/{id}` | 删除版本（连文件一起删；删的若是当前版本，自动把同客户端最新的一条顶上来） |

管理页能看到：每个版本的大小 / 下载次数 / 发布时间、两条线各自当前的版本、
累计客户端检查次数、内网与公网地址、网页版清单地址。

## 环境变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `DATA_DIR` | `/data` | 数据库、签名密钥、程序包都在这一个目录 |
| `BIND` | `0.0.0.0:8080` | 监听地址；`--health` 子命令也读它 |
| `ADMIN_TOKEN` | 空 | 管理页口令。**不设就登不进管理页**（清单和下载照常可用） |
| `LAN_BASE` | 空 | 内网访问前缀，用来拼 `lan_url` |
| `PUBLIC_BASE` | 空 | 公网访问前缀，用来拼 `public_url` |
| `CHANNEL` | `stable` | 写进清单的 `channel` 字段 |

## 部署

```bash
docker compose pull && docker compose up -d
```

`docker-compose.yml` 里已经写好端口映射（`50205:8080`）、卷（`./data:/data`）、
两个地址前缀和健康检查，镜像名跟 Python 版保持一致，所以 NAS 上**不用改任何东西**。

首次部署前只改一处：把 `ADMIN_TOKEN` 换成自己的口令。

```yaml
environment:
  ADMIN_TOKEN: "换成一个你自己记得住的口令"
```

> 口令以明文出现在 compose 文件里，而 compose 是进仓库的。更稳妥的做法是
> 新建一个**不进仓库**的 `.env`（`.gitignore` 已排除），compose 里写
> `ADMIN_TOKEN: ${ADMIN_TOKEN}`。

### 数据迁移（从 Python 版换过来）

**直接沿用原来的 `data/` 目录就行。** 启动时会自动给 `releases` 表补上 `client` 列，
历史记录全部归到 `python` 线，老客户端继续能更新，什么都不用做。

这个路径实测过：旧库里有 Python 版 v1.1.2，换成本服务启动后 `/latest.json` 正常返回
1.1.2，`/latest.json?client=web` 返回 404（网页版还没发过）—— 两条线互不影响。

### 备份

只需要备份 `data/` 一个目录：

```
data/updates.db     版本记录与计数
data/secret.key     登录 Cookie 的签名密钥（丢了只是所有人重新登录一次）
data/packages/      程序包（丢了客户端下载会 404，但清单还在）
```

## 日常发版流程

1. 打开 `http://<NAS>:50205/admin`，口令登录；
2. 选客户端（Python 版 / 网页版）、填版本号（形如 `0.2.0`）、写更新说明、选文件，提交；
   - 版本号格式不对会被拒绝；
   - 文件超过 300MB 会被拒绝；
   - 程序包的文件名由服务端拼（`bkstools-<版本>.exe`），不采信上传的文件名；
   - 会自动算 sha256 并判断是「安装包」还是「程序」（Inno 标记检测），回显里能看到；
3. 回到列表核对：新版本在对应客户端那条线上带「当前」标签；
4. `curl http://<NAS>:50205/latest.json?client=web` 看一眼清单里的 size / sha256 对不对；
5. 客户端那边：新版会在启动 8 秒后静默检查并下载，点「重启并更新」即生效。

发错版本不用慌：列表里对任意历史版本点「切回」就是重新发布它，客户端下次检查就回到旧版。

### 镜像发布

推到 `main` 且改动了 `src/**`、`Cargo.toml`、`Cargo.lock`、`Dockerfile`、workflow 时，
GitHub Actions 自动构建 `linux/amd64 + linux/arm64` 双架构镜像并推到：

```
ghcr.io/wukaibo98/beikeshi-updates:latest          # 默认分支
ghcr.io/wukaibo98/beikeshi-updates:<git tag>       # 打 tag 时
ghcr.io/wukaibo98/beikeshi-updates:sha-<7位>       # 每次
```

用的是仓库自带的 `GITHUB_TOKEN`，不需要配任何密钥。也可以在 Actions 页面手动触发。

## 本地开发

```bash
# 前台跑，数据放临时目录；地址前缀都配上，清单里才看得到完整 url
DATA_DIR=/tmp/bks-dev ADMIN_TOKEN=test123 BIND=127.0.0.1:8080 \
LAN_BASE=http://127.0.0.1:8080 PUBLIC_BASE=https://bks.example.com \
cargo run

# 探活（scratch 镜像里没有 curl/shell，所以做成了子命令）
BIND=127.0.0.1:8080 cargo run -- --health
cargo run -- --version
```

冒烟要点见 [DEV.md §9](DEV.md#9-改完代码怎么验证)，一条条 curl 过去大概两分钟。

## 目录

```
src/
├─ main.rs    配置、路由、清单生成、下载、健康检查
├─ admin.rs   管理页、登录、上传、发布、删除
├─ db.rs      SQLite 存取（表结构与 Python 版兼容，多了 client 列）
├─ auth.rs    口令校验与 Cookie 签名
└─ html.rs    管理页 / 登录页模板
Dockerfile                     多阶段构建 → scratch，镜像里只有一个二进制
docker-compose.yml             部署（NAS 上直接用这个）
.github/workflows/docker.yml   推 main 自动构建多架构镜像并推 ghcr
```

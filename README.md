# 贝可施更新分发服务

给「贝可施客服小工具」发版本、发安装包的小服务。**一个静态二进制 + 一个 SQLite 文件**，
没有别的依赖：NAS 上不用装运行时，镜像里只有一个 2.6MB 的可执行文件（整镜像 4.92MB）。

```bash
GET /latest.json?client=web      # 客户端拿清单
GET /packages/bkstools-0.5.0.exe # 下包
GET /admin                       # 管理页：概览 / 表单登记 / 版本更新 / 登录管理
```

线上跑的是 **0.6.0**；源码已经是 **1.6.6**（账号密码登录 + 物流快递字段 + 按快递分工作表 + 心跳在线状态，**尚未部署**，跟客户端版本无关）。
NAS 上容器名 `bks-updates`，容器内端口 8080，NAS 映射到 **50205**；公网经 Lucky 反代到 `https://bks.beikeshi.cn:54321`。

### 账号密码登录改造记录（2026-10-31，ChatGPT 修改，未发布）

- 客服以姓名作为账号，使用独立密码登录；密码以 Argon2 哈希存储。管理员可设初始密码和重置密码，客服可在客户端修改密码。
- 密码重置、改名、停用/启用都会递增账号会话版本，使旧令牌失效；登录失败按来源 IP 限制尝试次数。
- 旧客服账号升级后没有密码哈希，需要管理员逐个设置初始密码。服务端改动尚未部署，客户端也尚未发新版。
- `cargo test --release` **38 项全过**；登录/改密/限流/旧 token 失效已在本地逐条实测；尚未部署。

v0.6.0 把「表单预览」从**记录列表**改成**网格**：格子可以直接改（改的是登记数据，
不是模板字段），改完失焦自动存；原来的记录列表整体搬到【上传记录】里去看。
v0.5.0 给每个模板加了三个按钮（**编辑** / **上传记录** / **导出**），
并给「已删除的模板」加了**永久删除**。见「表单预览的网格」与「永久删除模板」。

### 源码改进记录（2026-10-31，ChatGPT 修改，尚未部署）

- SQLite 单连接全局锁改为最多 8 个连接的连接池；数据库启用 WAL，每条连接设置 `NORMAL` 同步模式和 5 秒 busy timeout。
- 数据库查询改为从池中取连接；xlsx 导出查询、表格转换和文件生成放入 `spawn_blocking`，避免大导出占用 Tokio 异步工作线程。
- `cargo test --release` **38 项全过**；12 并发导出压测全 200、无 panic；改动尚未部署到 NAS。

> **部署方式**：镜像**在本机构建**（`docker build` → `docker save | gzip` → `ssh cat` 传到 NAS
> → `docker load` → `compose up -d`），不再从 ghcr 拉。步骤见下面「部署到 NAS」。
> ghcr 上那个镜像只作备用（要多架构时才用）。

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
| 镜像体积 | ~60MB | **4.92MB**（实测，v0.2.0；加表单与 xlsx 导出之前是 3.66MB） |
| 内存占用 | 60~80MB | ~5MB |
| 启动 | 1~2 秒（import 一堆包） | <50ms |
| NAS 上要装的东西 | Python 运行时 + 依赖 | 什么都不用 |

接口、环境变量、镜像名、数据库表结构**全部保持兼容**，NAS 上原地替换即可。

## 只服务新版客户端

**旧 Python 版已停发**，服务端只有一条版本线：

```
GET /latest.json?client=web    → 正常返回清单
GET /latest.json               → 404「旧版客户端已停止发布，请换用新版」
GET /latest.json?client=python → 404「未知客户端：python」
```

为什么对旧客户端回 404 而不是把新版的清单发给它：**旧客户端的更新逻辑里没有
`client` 校验**，拿到清单就会把 `bkstools-*.exe` 当成自己的新版下载下来、
替换掉自己。所以宁可让它显示"服务器还没有发布版本"（它就会安静地不更新），
也不要把新版的包喂给一个已经不维护的程序。

- 上传统一按一条线走，文件名固定 `bkstools-<版本>.exe`（不再有 `BeikeshiCS-` 前缀）；
- 清单里的 `client` 字段仍然是 `web`，新版客户端会校验它（对不上就拒绝安装，双保险）；
- 库里还留着已停发的 Python 版历史记录，管理页给它们打「旧版·已停发」标记：
  不能切回、也不占"当前版本"的位置，只供查历史 / 删掉。

## 接口

### 对外（客户端用，无需登录）

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/health` | 探活，返回 `ok` |
| GET | `/latest.json?client=web` | 版本清单，**每次现生成**（发布/回滚后立刻生效）；其它取值一律 404 |
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
| POST | `/admin/upload` | multipart：`version` / `notes` / `file`，**上传即发布**。单个包上限 300MB（v0.3.1 修了"大文件传到一半被截断"的问题） |
| POST | `/admin/publish/{id}` | 把某个历史版本切回"当前"（回滚） |
| POST | `/admin/delete/{id}` | 删除版本（连文件一起删；删的若是当前版本，自动把同客户端最新的一条顶上来） |

管理页能看到：每个版本的大小 / 下载次数 / 发布时间、两条线各自当前的版本、
累计客户端检查次数、内网与公网地址、网页版清单地址。

### 表单收集（客户端用）

客服在客户端填写的表单走这一组。**模板的主编辑器在客户端**，
服务端另有一套**网页版编辑器**（v0.5 起）作为应急/微调通道，见「表单预览的三个按钮」。

| 方法 | 路径 | 请求 / 说明 |
|---|---|---|
| POST | `/form/login` | `{"name":"张三","password":"…"}` → 姓名账号 + 密码验证，发 30 天 token（放 `X-Bks-Form` 头） |
| POST | `/form/password/change` | 当前登录人验证旧密码并设置新密码；递增会话版本，旧令牌失效 |
| POST | `/form/edit-login` | `{"token":"<管理口令>"}` → 换 7 天 `edit_token`（放 `X-Bks-Edit` 头）。**口令只在服务端比对，客户端不落盘** |
| GET | `/form/templates` | 启用中的模板；带 `?all=1` 要 `edit_token`（编辑器要看到停用项） |
| POST | `/form/template/save` | 存模板，要 `edit_token`。`base_version` 是乐观锁，对不上返回 409 且**不改库** |
| POST | `/form/template/toggle` | 停用 / 启用模板，要 `edit_token` |
| POST | `/form/submit` | 提交，要 `X-Bks-Form`。详见下方三条硬规则 |
| GET | `/form/mine` | 我自己最近提交的记录（确认"真交上去了"，离线补交后尤其需要） |

字段类型 6 种：`text` / `textarea` / `number` / `select` / `date` / `name`（姓名，会自动去名单里核名字）。
模板 schema 长这样：

```json
{
  "title": "补发申请",
  "fields": [
    {"key":"order_no","label":"订单号","type":"text","required":true},
    {"key":"reason","label":"补发原因","type":"select","required":true,
     "options":["少发","破损","错发"]},
    {"key":"day","label":"发生日期","type":"date"}
  ]
}
```

**`/form/submit` 的三条硬规则**

1. `submission_id` 由客户端生成一次，所有重试用同一个；服务端
   `ON CONFLICT DO NOTHING`，重复提交返回 `duplicate` **并且算成功**。
   网络超时但服务器其实已入库时，重试不会产生第二条。
2. 服务端按 `template_version` 取出**当时的字段定义**再复验一遍（不信客户端）。
   离线几天后拿旧版本提交照样能收，只是记录里如实存旧版本号。
3. 每条提交都冻结一份字段定义快照（`template_snapshot`）。
   模板改了以后，导出的列按提交自带的快照取 —— 这条不守，历史数据会串列且没法修。

### 管理页是标签页（v0.4 起两级）

`GET /admin` 一次返回全部内容，页面上有**四个顶层标签**，
其中两个下面还有**二级标签**（切标签不刷新，URL 同步成 `?tab=&sub=`）：

| `?tab=` | `?sub=` | 内容 |
|---|---|---|
| `overview` | — | **谁在连服务器**（最近活动：谁/机器、IP、最后动作、时间）+ 概况数字 + 地址 |
| `register` | `preview` | **表单预览**：按模板分组列出登记。每组三个按钮：**编辑** / **上传记录** / **导出**，见下 |
| `register` | `templates` | **模板管理**：停用 / 启用 / **删除**（删是软删除，见下）+ 已删除的模板清单（可**永久删除**） |
| `versions` | — | 发布新版本 + 版本列表 |
| `login` | `clients` | **客户端**：谁能填表（姓名白名单）—— 添加 / 停用 / 改名 / 删除 |
| `login` | `admins` | **管理端**：谁能登进管理页 —— 添加 / 改口令 / 停用 / 删除 |

二级标签没给就用该顶层标签的第一项（`register` → `preview`，`login` → `clients`）。
标签本身是 `<a href="?tab=&sub=">`，没有 JS 时点它照样整页跳转。

旧地址 `/admin/forms` 会 302 到 `?tab=register`，书签不会失效。

### 表单预览：网格 + 三个按钮（v0.5 起按钮，v0.6 起网格）

「表单预览」里**每一个模板**（哪怕一条登记都没填过）都是一个卡片：上面三个按钮，
下面一张**可以直接改的网格**。

| 按钮 | 做什么 | 落地方式 |
|---|---|---|
| **编辑** | 在网页里直接改这个模板的**表格结构**（有哪些列、每列什么类型、必填与否、下拉选项、列的顺序），保存后客户端下次拉取就是新表 | `GET /admin/forms/template/{id}/edit` 取 HTML 片段弹层；保存走 `POST /admin/forms/template/save`（带 `base_version` 乐观锁，对不上返回 409 且**不改库**） |
| **上传记录** | **新窗口**打开这个模板的上传记录（只读列表），只显示**最近 200 条** | `GET /admin/forms/template/{id}/records`（独立页面，不是弹层），`target="_blank"` |
| **导出** | 导出这个模板的全部登记为 **xlsx** | `GET /admin/forms/export.xlsx?template={id}` |

工具条上那个按钮改叫「**导出当前筛选**」—— 它导的是**当前筛选条件下**的登记（可跨模板）。
想要「就这一个模板、全部历史」，点模板右上角的【导出】。没选模板就点它，会提示改用模板上的【导出】
（跨模板导出的列会稀疏得没法看）。

#### 网格改的是什么（v0.6 起）

**改登记数据，不改模板字段。** 列的来源是**当前模板的字段定义**（不是每条登记自带的快照），
所以同一模板的所有行才对得齐：

- 前三列 `#` / **提交人** / **填写时间**是灰底**只读**的元数据，不给改；
- 后面每一列对应模板的一个字段，控件按类型来：`select` → 下拉、`date` → 日期选择、
  `number` → 数字框、其余 → 单行文本；
- 老登记里多出来、当前模板已经没有的字段，会附在最后并标一个「**旧**」—— 那一列**只读**，
  改回去会跟别的行对不上，但数据一个字节都不丢（导出和上传记录里都看得到）；
- **改完一个格子失焦就自动存**（`POST /admin/forms/submission/grid-save`），
  行首亮橙点表示"还没存上"，存好了状态列显示 `✓ 时间`；
- **没有"新增一行"** —— 填新登记是客户端填表的职责，网页端只改已经填过的；
- 网格最多显示 **200 条**（跟上传记录一致），超了在卡片标题旁提示。

**没开 JavaScript 也照样能改**：格子退化成纯文本，行尾那个「**改**」链接整页打开登记编辑表单
（`?rec={id}`，服务端照样渲染），保存走原来的 `POST /admin/forms/submission/save`。
两条路共用同一个校验函数（`apply_submission_edit`）：只改 `data_json`、按提交那一刻的
快照校验、没提到的字段原样保留。

**关于「编辑」**：这里改的是**表格结构**（schema），不是表格里的数据。
数据内容的修改就在网格里直接改，或点行尾「改」开弹层 —— 字段定义快照一动不动，改了就会串列。

**为什么不引入开源 Office**（常见想法，这里明确记一下取舍）：

- 要引入的是 **OnlyOffice / Collabora / LibreOffice headless** 这一类。OnlyOffice Document Server
  镜像 1–2 GB，还要额外 Postgres + Redis；LibreOffice headless 也在几百 MB 量级。NAS 上再压一套
  办公套件，等于给这台机器加一个**新的部署面、一组新的漏洞面**，为的是编辑一种**字段受限的表单结构**，
  投入产出不划算。
- 更关键的是**功能对不上**：本工具的模板只有 6 种字段类型（`text` / `textarea` / `number` /
  `select` / `date` / `name`），是**结构化表单**，不是任意 Excel 文件。用一套办公套件去编辑
  一张只有几列的表单，等于为了拧一颗螺丝先装一整个车间。
- 「改数据」这个真正的需求，v0.6 的网格已经满足了（单元格直接改、失焦即存、零新增部署面）。
- 结论：**暂不引入**。要真需要 Office 级体验，优先走「客户端里做」——客户端本来就有完整的模板编辑器
  （`/form/template/save`），字段定义、校验、版本都更完备，浏览器这一侧只保留应急开关和结构微调。

> **2026-10-29 更新（结论的前提变了）**：用户后来在绿联应用中心装了 **「在线文档」**，
> 那其实就是**完整的 OnlyOffice Document Server**（原生安装，不是 Docker），已经在 NAS 上跑着，
> `/ugoffice/healthcheck` 返回 `true`。也就是说**"NAS 上没有 Office"这个前提已经不成立**，
> 上面"不引入"的结论只针对"再往 NAS 里压一套新的"。
> 接它有三条路线，各有一个坎，详细实测数据与对比见 [DEV.md §4.18](DEV.md)。
> **目前尚未接入**（用户决定先存档）。另有一条语义红线要走之前必看：
> 即使用 xlsx 往返，也**只能改登记数据，不能改模板结构** —— 后者会破坏「快照冻结」，
> 历史数据一旦串列就无法修复。

### 删模板 = 软删除（历史表格不受影响）

「删除」不是真删：

- 模板这一行**留在库里**，只打一个 `deleted_at` 时间戳，名字改成「原名（已删除）」
  （`name` 上有 UNIQUE，不改名就没法用同一个名字重新建一个模板 —— "删了重做"是最常见的用法）；
- **已经填过的登记一条都不动**，导出出来的表格也照旧能导；
- 历史登记里显示的仍是原来那张表（`客户需求表（已删除）`），不会变成一个光秃秃的 `#3`；
- 客户端下次拉取就看不到这个模板了；往已删模板**提交新登记会被拒**。

「已删除的模板」清单列在「模板管理」下面，让人看得到历史还在。

### 永久删除模板（v0.5 起）

软删除的模板可以在「模板管理 → 已删除的模板」里**永久删除**（`POST /admin/forms/template/purge`），
点之前有二次确认。

永久删除做三件事：

1. **先把模板名冗余写进历史登记** —— `form_submissions.template_name` 这一列就是为它准备的。
   模板行真删了以后，历史登记仍然认得出「这条是哪个表填的」，导出的表格（靠 `template_snapshot`）
   也照常。
2. **删掉这个模板的字段定义历史版本**（`form_template_versions` 里该模板的行）。
3. **删掉模板行本身**（`form_templates`）。

**历史登记一条都不删。** 这是硬规矩：删模板是「不想要这张表了」，不是「连填过的记录一起抹掉」。

前置条件：模板**必须先软删除过**（`deleted_at > 0`）。没删过的直接调永久删除会返回
`not_deleted` —— 免得手滑把还在用的表直接抹了。

### 管理端人员与口令

`ADMIN_TOKEN` 环境变量那个口令仍然是**超级管理员**（在 compose 的 `.env` 里，改它要重启容器）。
除它以外，还能在「登录管理 → 管理端」里给别的人各配一个口令：

- 口令**只存 sha256 哈希**，库里看不到原文，页面也不显示；
- 登录时环境变量口令和库里的人员口令**都认**；
- 忘了口令就用环境变量口令登进来，在「管理端」里「改口令」重置；
- 停用后这个人立刻登不进来（已登录的 Cookie 还能用到他下次请求，之后失效）。

### 表单收集（管理页，沿用 admin Cookie）

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/admin?tab=register&sub=preview` | 表单预览（按模板分组，可筛选） |
| GET | `/admin/forms/submission/{id}/edit` | 单条登记的**可编辑表单**（HTML 片段，弹层用） |
| POST | `/admin/forms/submission/save` | 改一条登记的**内容**（字段定义快照不动，改了列就串了） |
| GET | `/admin?tab=register&sub=templates` | 模板清单（能停用、能删除） |
| GET | `/admin?tab=login&sub=clients` | 客户端名单 |
| GET | `/admin?tab=login&sub=admins` | 管理端人员 |
| POST | `/admin/users/save` | 加人 / 改名。**加人必须有界面**，否则新人用不了等于只有你能维护 |
| POST | `/admin/users/toggle` | 停用 / 启用（停用后登录失败，历史提交保留） |
| POST | `/admin/users/delete` | 删除。**历史提交不动**（记录里冗余存了姓名，删了人还认得出是谁填的） |
| POST | `/admin/admins/save` | 加管理端人员 / 改口令（两次口令要一样） |
| POST | `/admin/admins/token` | 只改口令，不改名字 |
| POST | `/admin/admins/toggle` | 停用 / 启用管理端人员 |
| POST | `/admin/admins/delete` | 删除管理端人员 |
| POST | `/admin/forms/template/toggle` | 模板应急开关：改坏了可以在这里停掉，客户端下次拉取就不再显示 |
| POST | `/admin/forms/template/delete` | **软删除**模板（历史登记与已导出表格不受影响） |
| GET | `/admin/forms/template/{id}/edit` | 模板编辑器的 **HTML 片段**（弹层用；无 JS 时走 `?tpl_edit={id}` 内联） |
| GET | `/admin/forms/template/{id}/records` | 该模板的**上传记录**独立页面，**只显示最近 200 条** |
| POST | `/admin/forms/template/save` | 保存模板结构（网页版编辑器）。带 `base_version` 乐观锁，对不上返回 409 且**不改库** |
| POST | `/admin/forms/template/purge` | **永久删除**已软删除的模板（历史登记保留，模板名已冗余进登记） |
| GET | `/admin/forms/export.xlsx` | 导出，**必须先选模板**：一次只导一个模板，否则列会稀疏得没法看。带 `?template={id}` 时导出该模板全部登记 |

## 环境变量

| 变量 | 默认 | 说明 |
|---|---|---|
| `DATA_DIR` | `/data` | 数据库、签名密钥、程序包都在这一个目录 |
| `BIND` | `0.0.0.0:8080` | 监听地址；`--health` 子命令也读它 |
| `ADMIN_TOKEN` | 空 | 管理页口令。**不设就登不进管理页**（清单和下载照常可用） |
| `LAN_BASE` | 空 | 内网访问前缀，用来拼 `lan_url` |
| `PUBLIC_BASE` | 空 | 公网访问前缀，用来拼 `public_url` |
| `CHANNEL` | `stable` | 写进清单的 `channel` 字段 |
| `TZ_OFFSET` | `480` | 管理页显示时间用的时区偏移（分钟）。默认 +8 小时；scratch 镜像没有 tzdata，只能靠它 |

### "谁在连服务器"是怎么来的

客户端每次**检查更新**（`/latest.json`）和**表单登录 / 提交**都会在服务端记一笔
（`activity` 表：动作、姓名、IP、机器标识、时间）。概览页按「IP + 机器标识」聚合成
一台机器，只显示最后一次动作；15 分钟内有动作的标绿点「在线」。

- **没有心跳**，所以这不是真正的在线状态 —— 时间一律如实显示"N 分钟前"。
- IP 优先取 `X-Forwarded-For`（Lucky 反代后面的真实 IP），没有再用连接的地址。
- 只留 7 天，自动清理。
- 客户端还没传机器标识（要等客户端那一半做完），现在主要靠 IP 区分。

## 部署

把 `docker-compose.yml` + `.env.example` 放到 NAS 上同一个目录（例如 `docker/bks-updates/`），然后：

```bash
cp .env.example .env      # 1. 填口令
vi .env                   #    ADMIN_TOKEN=一个长口令
docker compose pull       # 2. 拉镜像（公开包，不用 docker login）
docker compose up -d      # 3. 起服务
docker compose ps         #    等 Status 从 (health: starting) 变成 (healthy)
```

`docker-compose.yml` 里已经写好端口映射（`50205:8080`）、卷（`./data:/data`）、
两个地址前缀、健康检查和日志上限，镜像名跟 Python 版保持一致，**别的地方都不用改**。

口令走 `.env`（已在 `.gitignore` 里），不写进 compose —— compose 是要进仓库的。
没建 `.env` 时 `docker compose` 会直接报
`required variable ADMIN_TOKEN is missing a value`，不会稀里糊涂起一个登不进管理页的服务：

```yaml
environment:
  ADMIN_TOKEN: "${ADMIN_TOKEN:?请在 .env 里设置 ADMIN_TOKEN}"
```

> 之前的口令曾经明文写在 compose 里、也跟着进了仓库，**换成这套之后记得把旧口令改掉**。

### 数据迁移（从 Python 版换过来）

**直接沿用原来的 `data/` 目录就行。** 启动时会自动给 `releases` 表补上 `client` 列，
老记录全部落成 `python` 线，在管理页里显示为「旧版·已停发」，不会被当成当前版本。

实测过：旧库里有 Python 版 v1.1.2，换成本服务启动后 `/latest.json?client=web` 返回 404
（新版还没发过），`/latest.json` 返回「旧版客户端已停止发布」—— 老记录不会漏给新版客户端。

### 备份

只需要备份 `data/` 一个目录：

```
data/updates.db     版本记录与计数
data/secret.key     登录 Cookie 的签名密钥（丢了只是所有人重新登录一次）
data/packages/      程序包（丢了客户端下载会 404，但清单还在）
```

## 日常发版流程

1. 打开 `http://<NAS>:50205/admin`，口令登录；
2. 填版本号（形如 `0.2.0`）、写更新说明、选文件，提交（不再选客户端，只有一条线）；
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

### 本地构建镜像（国内要换镜像源）

Docker Hub 直连会被拦，`FROM rust:1-alpine` 会卡在 `DeadlineExceeded`。
基础镜像留了 build-arg，换源不用改文件：

```bash
docker build --platform linux/amd64 \
  --build-arg RUST_IMAGE=docker.m.daocloud.io/library/rust:1-alpine \
  -t bks-updates:local .

docker run -d --name bks-local -p 18081:8080 \
  -e ADMIN_TOKEN=test123 -e LAN_BASE=http://127.0.0.1:18081 \
  -v /tmp/bks-data:/data bks-updates:local

docker exec bks-local /bks-updates --health; echo $?    # 0
```

实测：Intel Mac 上 1 分 16 秒出镜像，**3.66MB**；跑起来内存 **4.5MiB**。
（scratch 镜像里没有 shell，探活直接 `docker exec <容器> /bks-updates --health`。）

## 目录

```
src/
├─ main.rs    配置、路由、清单生成、下载、健康检查
├─ admin.rs   管理页、登录、上传、发布、删除
├─ db.rs      SQLite 存取（表结构与 Python 版兼容，client 列只用于标记旧记录）
├─ auth.rs    口令校验、Cookie 签名、表单 token / 编辑 token
├─ forms.rs   表单：schema 校验、姓名登录、模板存取、提交、管理页、导出
├─ xlsx.rs    提交记录导出（列并集、数字列、sheet 名清洗）
├─ html.rs    登录 / 分发 / 表单三个页面模板
└─ tests.rs   单元测试（幂等、快照冻结、乐观锁、导出列……）
Dockerfile                     多阶段构建 → scratch，镜像里只有一个二进制
docker-compose.yml             部署（NAS 上直接用这个）
.github/workflows/docker.yml   推 main 自动构建多架构镜像并推 ghcr
```

详细设计与字段说明见客户端仓库的 `FORMS.md`。


## 2026-10-31 功能修改记录（ChatGPT 修改，未部署）

- 概览页改为完整客户端账号名单及在线/离线状态；客户端每分钟上报设备心跳，超过 3 分钟无心跳显示离线，同账号多设备分别列出。
- 模板增加「物流快递」复合字段（单号 + 公司），单号前缀尝试识别，物流公司可手动修改；数字号段有交叉，无法可靠区分时不自动选。
- 物流字段可勾选导出按快递公司分工作表；未识别/未填写归到单独工作表。
- 表单预览卡片不再内嵌填写记录；「上传记录」页提供最近 200 条的可编辑网格。
- 服务端 Rust `cargo check --offline` 已通过；尚未运行自动化测试或部署。识别规则调研与实现边界见 [DEV.md](DEV.md) 的「ChatGPT 修改记录」。

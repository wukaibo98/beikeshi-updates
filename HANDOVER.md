# 贝可施更新分发服务 — 交接文档

> 写给下一个接手的人/智能体。读完这份，你能不改错地把这套发版链路继续用下去。
> 实现细节见 [DEV.md](DEV.md)，部署与发版流程见 [README.md](README.md)。

**当前状态：服务端 v0.2.0。** 更新分发那条链路已经**真的跑在 NAS 上**（2026-10-28 15:52 部署完成，
`docker compose pull && up -d`，容器内 `bks-updates 0.1.1` → 本次升到 0.2.0）。
v0.2.0 新增**表单收集**（姓名登录 + 模板存取 + 提交 + 导出 xlsx）：服务端这一半
代码、单测（12 项）、curl 冒烟（36 项）都已完成并在本机验过，**客户端那一半还没动**。
下一步是客户端的填写页与模板编辑器 —— 见 §6.3。**

---

## 1. 五分钟上手

```bash
# cargo 不在 PATH，在 ~/.cargo/bin
~/.cargo/bin/cargo --version        # 1.99.1（Cargo.toml 里声明最低 1.75）

cd ~/AI/WorkBuddy/beikeshi-updates-rs

# 1. 编译（依赖已缓存，加 --offline 可以完全不联网）
~/.cargo/bin/cargo build --release --offline

# 2. 跑起来（数据放临时目录，别碰 ~/AI/WorkBuddy/beikeshi-updates 里那份真数据）
DATA_DIR=/tmp/bks-dev ADMIN_TOKEN=test123 BIND=127.0.0.1:18080 \
LAN_BASE=http://127.0.0.1:18080 PUBLIC_BASE=https://bks.example.com \
  target/release/bks-updates

# 3. 冒烟（完整清单见 DEV.md §9）
curl -s http://127.0.0.1:18080/health
curl -s -w " %{http_code}\n" http://127.0.0.1:18080/latest.json    # 期望 404
open http://127.0.0.1:18080/admin                                  # 登录页

# 4. 子命令
BIND=127.0.0.1:18080 target/release/bks-updates --health; echo $?   # 0
target/release/bks-updates --version                               # bks-updates 0.1.0
```

### 代码地图

| 文件 | 职责 | 改动频率 |
|---|---|---|
| `src/main.rs` | 配置、路由、清单生成、下载、`--health`/`--version`、包类型检测 | 中（清单字段变化时） |
| `src/admin.rs` | 管理页、登录、上传、发布、删除 | 中 |
| `src/db.rs` | 两张表的全部 SQL，**`is_current` 必须按 client 收窄** | 低（改错就是事故） |
| `src/auth.rs` | HMAC Cookie、常数时间比较 | 几乎不动 |
| `src/forms.rs` | 表单：schema 校验、姓名登录、模板存取、提交、表单管理页、导出 | **高（客户端联调期）** |
| `src/xlsx.rs` | 导出 xlsx（列并集 / 数字列 / sheet 名清洗） | 低 |
| `src/tests.rs` | 单元测试（幂等、快照冻结、乐观锁、导出列） | 中（加功能就要加） |
| `src/html.rs` | 登录页 / 分发页 / 表单页模板（`%%占位%%`） | 低（只想改样式时动它） |
| `Dockerfile` | 多阶段 → scratch | 低 |
| `docker-compose.yml` | NAS 部署（端口 / 卷 / 环境变量 / healthcheck） | 低 |
| `.github/workflows/docker.yml` | 推 main 自动构建多架构镜像 → ghcr | 低 |

---

## 2. 环境注意事项（先读，能少踩坑）

1. **本机有 Docker Desktop（Intel Mac，x86_64），能本地构建镜像**，但 **Docker Hub 直连被拦**
   （`load metadata for docker.io/library/rust:1-alpine` → `DeadlineExceeded`）。
   换镜像源即可，不用改 Dockerfile：

   ```bash
   docker build --platform linux/amd64 \
     --build-arg RUST_IMAGE=docker.m.daocloud.io/library/rust:1-alpine \
     -t bks-updates:local .
   ```

   实测 1 分 16 秒出镜像，**3.66MB**；容器跑起来内存 **4.5MiB**。
   日常改代码的验证还是"跑二进制 + curl"更快，不必每次都构建镜像。
   注册表 `docker.m.daocloud.io` 可用（`dockerproxy.net`、`docker.1panel.live` 也通）。
   注意：本地构建**只是产出镜像**，要让它出现在 ghcr 上仍得走 Actions 或 `docker push`（见 §6.1）。
2. **`cargo` 不在 PATH**，用 `~/.cargo/bin/cargo`。`clippy` 没装（`cargo clippy` 会报
   toolchain 里没有），想用先 `rustup component add clippy`。
3. **不要拿真数据目录做实验**。真数据在 NAS 上（`./data`，通过卷挂进容器）；
   本机 `../beikeshi-updates/data/` 那份是 Python 版遗留的测试数据。实验用 `DATA_DIR=/tmp/...`。
4. **`--health` 读 `BIND`**：不设 `BIND` 时会去连 `127.0.0.1:8080`，
   和容器/本地实际端口不一致就返回 1（这不是 bug，是探针设计如此）。
5. **端口有三层**：容器内 `8080` → NAS 映射 `50205` → 公网 Lucky 反代 `:54321`。
   改任何一层都要同时改客户端里的两个地址常量（见 DEV §11）。
6. **NAS 上镜像名写死**在 `docker-compose.yml` 里：`ghcr.io/wukaibo98/beikeshi-updates`。
   而 workflow 用 `${{ github.repository }}` 生成镜像名 —— **仓库名必须是 `beikeshi-updates`**，
   改名会导致 Actions 推的镜像和 compose 拉的不一致。
7. **ghcr 包归属的坑（已经踩过一次，2026-10-28）**：旧仓库删了以后，同名包
   `beikeshi-updates` 成了"孤儿"，还指着已不存在的仓库。首次构建到最后的 push 步骤直接失败：

   ```
   ERROR: failed to push ghcr.io/wukaibo98/beikeshi-updates:latest:
          denied: permission_denied: write_package
   ```

   编译本身是成功的（17 分钟，双架构 QEMU），只是没有写这个包的权限。
   解法：把这个孤儿包删掉，重跑一次构建（新包会归属当前仓库就正常了）：

   ```bash
   curl -X DELETE -H "Authorization: Bearer <PAT 需要 delete:packages 权限>" \
     https://api.github.com/user/packages/container/beikeshi-updates     # 204 就对了
   ```

   已按这个办法处理过（包删掉后重跑 run #2 成功，镜像已在 ghcr 上，见 §8）。
   以后再遇到同样的 403，八成还是包归属问题（比如又删了一次仓库）。
8. 管理页口令 `ADMIN_TOKEN` 目前写在 compose 里（明文、会进仓库）。**接手后第一件事**
   建议挪到不进仓库的 `.env`，见 §6 待办。

### 2.1 推送这本仓库要注意网络

这台机器上 **`github.com`（git 用的 443)经常连不上，而 `api.github.com` 一直通**。
表现是 `git push` 卡 75 秒后报 `Failed to connect to github.com port 443`。两条路：

```bash
# 通路时：正常推
git push

# 不通时：走 REST API 全量快照推送（自动遵守 .gitignore）
GH_TOKEN=ghp_xxx python3 ~/AI/WorkBuddy/tools/gh_push.py . <owner>/<repo> "提交信息"
```

`gh_push.py` 会用远端 main 的 HEAD 当父提交，所以**它产生的提交在本地不存在** ——
一旦用过它，本地和远端历史就分叉了（内容一致，sha 不同）。等网络通的时候收一次：

```bash
git fetch origin main && git reset --hard origin/main
```

> 当前状态（2026-10-29 11:50）：**github.com 和 api.github.com 都通**（直连 0.8s），
> `git push` 正常。服务端仓库已对齐到远端真实历史（`e700db3` → `f8fa81f`），
> **本地与远端一致**，以后正常 `git push` 就行。
> 分叉的收尾办法（比 `reset --hard` 温和，保留本地提交）：
> `git fetch origin && git branch backup/pre-align-$(date +%Y%m%d) HEAD && git reset --hard origin/main && git cherry-pick <本地独有的提交>`。

### 2.2 没有 gh CLI 时的推法（`GIT_ASKPASS`）

本机没装 `gh`，钥匙串里默认也没有 GitHub 凭据。**macOS 钥匙串用 `security` 存容易匹配不上**
（`security add-internet-password -P https` 存进去 `ptcl` 字段仍是 0，git 的 osxkeychain helper
按 protocol=https 查不到，最后报 `could not read Password`）。用 `GIT_ASKPASS` 最省事：

```bash
export GH_TOKEN='ghp_xxx'
export GIT_ASKPASS=~/AI/WorkBuddy/tools/gh_askpass.sh   # 脚本里只有账号，令牌走环境变量
export GIT_TERMINAL_PROMPT=0
git fetch origin && git push origin main
```

`tools/gh_askpass.sh` 内容（**不含任何令牌**，令牌永远只从 `$GH_TOKEN` 读）：

```sh
#!/bin/sh
case "$1" in
  *Username*|*username*) echo "wukaibo98" ;;
  *) printf '%s\n' "$GH_TOKEN" ;;
esac
```

令牌撤销后本机就没有可用凭据了，重新推要再配一次。

---

## 3. 这个服务是怎么来的

| 阶段 | 内容 | 结果 |
|---|---|---|
| 1 | Python 版（FastAPI，420 行） | 功能完整、能用；镜像 ~60MB、内存 60~80MB、启动 1~2 秒 |
| 2 | 判断"要不要换" | 功能没问题，但部署形态不理想：NAS 上要 Python 运行时 + 一堆包 |
| 3 | Rust 重写（axum + rusqlite bundled） | 静态二进制 1.84MB → scratch 镜像 ~4MB、内存 ~5MB、启动 <50ms |
| 4 | 兼容性设计 | 数据库表结构、接口、环境变量、镜像名全部不变，NAS 原地替换 |
| 5 | 加 `client` 维度 | 客户端分 Python 版 / 网页版两条版本线，各自独立"当前版本" |
| 6 | 搬迁验证 | 用旧版建库发布 v1.1.2，换 Rust 版启动后 python 线正常返回、web 线 404 |
| 7 | 首次交付 | 补齐三份文档；去掉管理页那个永远显示「—」的"最近检查"死字段 |
| 8 | 2026-10-28 下午 | **去掉旧 Python 版**：管理页删掉客户端下拉与「两条版本线」提示，服务端只认 `client=web`，其它一律 404；旧记录打「旧版·已停发」、不可切回。顺手把管理页时间从 UTC 改成本地（`TZ_OFFSET`，默认 +480） |
| 9 | 2026-10-28 15:52 | **上 NAS**：SSH 装部署公钥 → `compose pull && up -d` → healthy。分发链路至此真机可用 |
| 10 | 2026-10-28 晚 | **表单收集 v0.2.0**（服务端一半）：4 张表、姓名登录、`edit_token`、模板乐观锁、提交幂等+快照、管理页、xlsx 导出；补上第一批单测（12 项） |
| 11 | 2026-10-28 22:25 | **v0.2.0 上 NAS**：停容器 → 备份 db/密钥 → `compose pull && up -d` → healthy。库里 4 张 form 表自动建好，自测数据已清干净（0 模板 / 0 用户 / 0 提交） |
| 12 | 2026-10-28 晚 | **v0.3.0 UI 改版**：管理页从两个独立页面改成**五个标签**（概览 / 版本 / 登记 / 模板 / 名单）；概览主打"谁在连服务器"（新增 `activity` 表）；名单加删除按钮 |
| 13 | 2026-10-28 23:27 | **v0.3.0 上 NAS**：备份 → 停容器 → `compose pull && up -d` → healthy，容器内 0.3.0，标签页与活动记录都正常 |

---

## 4. 线上形态（NAS）

```
NAS（飞牛 OS）
 ├─ docker compose（./data 卷）
 │   └─ 容器 bks-updates  ← 镜像 ghcr.io/wukaibo98/beikeshi-updates:latest
 │        ├─ 8080  清单 / 下载 / 管理页
 │        └─ /data
 │            ├─ updates.db    版本记录
 │            ├─ secret.key    登录签名
 │            └─ packages/     程序包（BeikeshiCS-*.exe / bkstools-*.exe）
 ├─ Lucky 反向代理：https://bks.beikeshi.cn:54321 → 内网 192.168.124.3:50205
 └─ 局域网直连：http://192.168.124.3:50205
```

客户端两个地址都配了，优先内网、回退公网；清单来源决定用 `lan_url` 还是 `public_url`。

---

## 5. 拍板过的决策（改之前先想清楚）

| 决策 | 内容 / 理由 |
|---|---|
| **只有一条版本线** | 2026-10-28 起旧 Python 版停发：只服务 `client=web`，缺参数/别的取值一律 404（旧客户端没有 client 校验，给它清单会把自己替换掉）。库里的 python 老记录只作历史，标记「旧版·已停发」，不能切回 |
| 镜像名不变 | 沿用 `beikeshi-updates`，NAS 上 `compose pull` 即完成替换，不用户改配置 |
| scratch 镜像 | 静态 musl 二进制，镜像里只有一个文件 —— "没东西可坏"是这套服务的核心诉求 |
| 上传即发布 | 没有草稿态。发版频率极低，少一个状态少一种故障 |
| 文件名服务端拼 | `BeikeshiCS-<版本>.exe` / `bkstools-<版本>.exe`，不采信上传名，从根上防穿越 |
| 口令只走环境变量 | 不入库、不写页面、不进镜像 |
| 无 JWT / 无 session 表 | 一个管理员，"HMAC 签名的随机串"够用，且重启不掉登录 |
| 清单不缓存 | 每次现生成，发布/回滚立即生效，也不会出现清单与库不一致 |
| 404 有语义 | "没发版"就是 404 + `{"error":...}`，客户端据此区分"没发版"与"连不上" |
| 页面上限 | 单包 300MB（客户端侧另有 200MB 上限，两边都不该随便放大） |
| 标记 Inno 用头尾各 4MB | 实测标记在文件 ~90% 位置，只扫开头会漏判 |
| 管理页不引前端框架 | 一屏表格 + 一个表单，手写 HTML 更快，也少一层依赖要维护 |
| **姓名登录只做白名单** | 只查"在不在名单里"，没有密码。防误填、能追溯是谁填的；**不防冒名**（知道同事名字就能填）。用户明确接受 |
| **模板在客户端编辑** | 服务端不做编辑器。日常高频的是"改一下字段"，打开客户端就能改比记 IP 进管理页划算。服务端只存/发/收/导 |
| **改模板要口令** | `edit_token`（7 天）。客户端发给所有客服，不设这道限制模板迟早被改乱，一改错所有客户端都跟着错 |
| **提交必须存字段快照** | 模板一改历史列就变了，没快照导出必串列且无法修复 —— 全套设计里最不能妥协的一条 |
| **对旧客户端一律 404** | 也适用于表单以外的判断：宁可让旧端安静地不更新，也不能给它可能有害的响应 |
| **导出一次只导一个模板** | 跨模板并排导列会稀疏得没法看；没选模板直接回页面提示 |
| **管理页做成标签页** | 用户要求"跟客户端类似"。内容一次渲染完 + 20 行 JS 切换（不刷新），URL 是状态唯一来源 |
| **"在线"只是最近 15 分钟有请求** | 没有心跳，不能假装实时。页面如实显示"N 分钟前" |
| **删人不删历史提交** | 提交记录冗余存了 `user_name`，删了人还认得出是谁填的。删提交等于毁账 |

---

## 6. 待办（按优先级）

### 6.1 P0 — 让这套 Rust 服务真的跑起来

> **2026-10-28 15:52 已完成。** compose 目录 `/volume1/docker/bkstools`（注意是 `.yaml`），
> `docker compose pull && up -d` 后容器 healthy，容器内 `--version` 0.1.1。
> 部署用的 SSH 公钥已装在 NAS 上（`~/.ssh/id_ed25519_bks`，注释 `bks-deploy@mac`），
> 之后 `ssh -i ~/.ssh/id_ed25519_bks "吴凯波@192.168.124.3"` 免密。
> 下面这几条按当前状态更新过，剩下唯一没做的是"同一个包能否被 Actions 覆盖"。

- [x] ~~推仓库到 GitHub~~ 已推（`wukaibo98/beikeshi-updates`，公开）；
- [x] ~~本地构建镜像~~ 已通（`docker build --build-arg RUST_IMAGE=docker.m.daocloud.io/library/rust:1-alpine`，
      3.66MB，容器内跑通登录/上传/清单/下载，见 §8）；
- [x] **镜像已上 ghcr**（2026-10-28 14:45）：删掉孤儿包后重跑 Actions（run #2，`66f83533`）成功。
      `ghcr.io/wukaibo98/beikeshi-updates:latest`
      digest `sha256:8c6ffcfa2c2cc6b4bb079883c1098349ce26b0ca06a8742b57985b640df410e4`，
      清单里有 `linux/amd64` + `linux/arm64` 两套。本机**匿名** `docker pull` 验证通过（3.68MB），
      跑起来 `/health` 200、`--version` 0.1.0；
- [x] ~~镜像构建~~ **改成本机构建**（2026-10-29 用户明确要求）：本地 `docker build` →
      `docker save` → `ssh cat` 传 NAS → `docker load`。镜像只有 ~5MB，压完 1.7MB，传输几乎瞬间。
      Actions 降级为备用（要多架构镜像时才用）。步骤见 DEV.md §8.0；
- [x] ~~NAS 部署~~ 已完成（2026-10-28 15:52），`/health` 200、管理页能登录、能看到历史版本；
- [x] ~~老库迁移~~ 实测：拿一份只有 `releases` 表的老库启动，四张 form 表自动建好，`releases` 数据原样保留；
- [ ] NAS 上 `ADMIN_TOKEN` 换成真口令（compose 里现在是部署时填的那个，改完要重启容器）；
- [x] ~~v0.2.0 上 NAS~~ 已完成（2026-10-28 22:25），容器内 `--version` 0.2.0；
- [x] ~~v0.3.0 标签页改版~~ 2026-10-28 23:27 已上 NAS，容器内 `--version` 0.3.0，
      标签栏 / 活动表 / 旧地址 302 都在 NAS 上复验过；
- [ ] 名单里现在有一个自测留下的「吴凯波」（id=1）。留着能用（就是你本人），
      不要就在 `?tab=users` 点删除；
- [ ] 去 `/admin?tab=users` 把客服姓名加进去（**姓名必须与本人说的完全一致**），加完客户端才能登录；

### 6.2 P0 — 把网页版客户端发上去

- [ ] 上传 `贝可施客服小工具-v0.2.0.exe`（**67,422,649 字节**，
      sha256 `7d0dde85eadefebcdbd09e185c39386a3e04ea06ab820a682a9e113dfc12ced1`），
      版本填 `0.2.0`（现在没有客户端可选了，只有一条线）；
- [ ] 核对清单：`/latest.json?client=web` 里的 `size` / `sha256` 与上面一致；
- [ ] 拿一台装旧版的机器实跑一遍：启动 8 秒后应该静默下载 → 出现「重启并更新」→
      点完新版起来（**这是整条链路唯一没有被真机验证过的一段**）；
- [ ] 顺带决定：Python 版 v1.1.2 的安装包（14,488,998 字节 / `da7b4e55…`）要不要一起发到 python 线。

### 6.3 P0 — 表单收集：客户端那一半（服务端已完成）

- [ ] **登记标签**：姓名登录（调 `/form/login`，token 存本地 30 天）、模板列表（本地缓存，断网也能填）；
- [ ] **填写页**：按 schema 渲染 5 种字段、**草稿自动存**（停手 800ms，存 `config\forms\drafts.json`，**不用 localStorage**）；
- [ ] **提交与离线队列**：`submission_id` 只生成一次 → 写 `outbox` → 后台重试（10s→1m→5m→10m 封顶），
      `duplicate` 当成功清队列；顶部常驻「待上传 N 条 / 离线」；
- [ ] **模板管理独立弹窗**（900×700）：口令登录换 `edit_token`、字段列表 + 属性 + 实时预览、↑↓ 排序；
- [ ] **断网演练**：拔网线填 5 条 → 恢复后全部到位且无重复；两个客户端同时改同一模板 → 后一个收到 409；
- [ ] 设计文档在客户端仓库的 `FORMS.md`（表结构、接口契约、字段类型、草稿与离线流程、导出规则）。

### 6.4 P1 — 安全与备份

- [x] `ADMIN_TOKEN` 已挪出 compose（2026-10-28）：改成 `${ADMIN_TOKEN:?...}` + `.env.example`，
      `.env` 在 `.gitignore` 里；本机用这套 compose 起过一整套（拉镜像 → healthy → 上传 → 读清单）验证通过。
      **NAS 上那份旧 compose 里的明文口令要顺手改掉**；
- [ ] NAS 上给 `data/` 加定时备份（`updates.db` + `secret.key` + `packages/` 三样就够）；
- [ ] 考虑在 Lucky 上给 `/admin` 加 IP 白名单或 basic auth（登录接口现在没有频率限制）。

### 6.5 P2 — 代码

- [x] **第一批自动化测试已加**（2026-10-28，`src/tests.rs` + `xlsx.rs` 内，`cargo test --release` 12 项）；
- [ ] 还没覆盖的：文件名白名单、版本号校验、清单拼接（lan/public 缺失的三种组合）、
      `detect_package` 的头尾扫描边界。要测这些得把纯逻辑抽到 `lib.rs`（现在是单 bin）；
- [ ] 下载计数现在每个请求 +1（客户端重试会虚高），可以按 IP/时间窗去重，或干脆改成"仅计数"；
- [ ] `Dockerfile` 的 `rust:1-alpine` 是浮动 tag，建议钉到具体小版本（可复现构建）；
- [ ] 管理页目前没有"仅上传不发布"，也没有删除前的二次确认以外的保护 —— 够用，先别加复杂度；
- [ ] 想要"灰度/多渠道"（stable/beta）的话，`channel` 字段已经在清单里了，但库里还没有对应列。

---

## 7. 排查手册（症状 → 先看哪里）

| 症状 | 先看 |
|---|---|
| 客户端说"连不上更新服务器" | `curl <地址>/health`；再看 `/latest.json?client=web` 是否 404（404 是"没发版"，不是连不上） |
| 清单 404 | 该 client 线上没有 `is_current=1` 的记录 —— 去管理页看有没有「当前」标签，点「切回」 |
| 清单 400 未知客户端 | 请求里的 `client` 不在 `python` / `web` 里（拼错、大小写之外的怪值） |
| 登录一直 401 | `ADMIN_TOKEN` 环境变量没设或和输入的不一致；`docker compose exec` 进去看不到环境变量就说明没生效 |
| 登录成功但立刻又回登录页 | 浏览器没存 Cookie（HttpOnly + SameSite=Lax）；换 `curl -c jar` 验证是服务端问题还是浏览器问题 |
| 上传后列表没有新版本 | 看 303 跳转带的是 `err=` 还是 `ok=`：版本号格式、>300MB、空文件、小于 64KB 都会走 `err` |
| 下载 404 | `packages/` 里文件被删了（删除版本会连文件一起删）；清单里那条记录却还在 → 重新上传 |
| 下载 400 文件名不合法 | 名字里有白名单外的字符（中文、空格、`..`） |
| 客户端下了但校验失败 | 清单 `sha256` 与文件不符：手工 `shasum -a 256` 对一遍；上传时是边写边算的，正常不会错，怀疑磁盘/网络中途出问题 |
| 容器起不来 | `docker logs bks-updates`；最常见是 `/data` 权限（卷挂载目录不可写）或 8080 被占 |
| healthcheck 一直 unhealthy | 容器里 `BIND` 必须与实际监听一致；`--health` 是 TCP 连自己 |
| 拉镜像 403 / not found | ghcr 包归属不对（见 §6.1 第二条），或包被设成私有而 NAS 没 `docker login` |
| 手工改库报 "readonly database" | `data/` 是 root:644，`吴凯波` 账号写不了。要么停容器后借 `ugreen/lucky:v3` 的 root shell 改权限（`docker run --rm --entrypoint /bin/sh -v .../data:/d ugreen/lucky:v3 -c "chmod 777 /d"`），要么别手工改 |
| 手工改库后发现表没了 | **别删 `updates.db-wal`**：WAL 模式下建表/写入先落在 wal 里，删掉等于丢掉最近所有变更。真要改就先停容器让 SQLite 正常 checkpoint |
| 镜像里的程序"什么都不做" | Dockerfile 的 `touch src/main.rs` 丢了，cargo 复用了空壳 `main` 的缓存 |
| **上传大文件报"连接被重置"** | v0.3.1 前是 axum 默认 body 限制 2MB 导致截断。升级到 0.3.1 即可；若还不行，检查 Lucky 的 `client_max_body_size` 之类上传限制 |
| 表单登录 403 | 姓名不在 `form_users` 或被停用；去 `/admin/forms` 最下面的名单里加（**姓名要完全一致**，含空格） |
| 模板保存 409 | 乐观锁：别人/别的机器先改了。刷新拿到新 `version` 再存；**库不会被改**，不会静默覆盖 |
| 模板保存 403 | `edit_token` 缺失或过期（7 天）。重新输一次管理口令 |
| 提交一直 422 | 看 `fields` 字段里的具体错误；若提示"表单版本已失效"，是客户端缓存的模板版本太旧 |
| 客户端"待上传"清不掉 | 服务端只回 200（`created`/`duplicate`）才算成功；若一直 4xx，检查 token 是否过期 |
| 导出重定向回页面 | 没选模板（一次只导一个），或该筛选下没有记录 —— 看地址栏 `err=` 的提示 |
| 概览"谁在连"一直是空的 | 客户端还没发过请求；或反代没传 `X-Forwarded-For`（IP 全是反代的） |
| 五个标签同时显示 / 切换无效 | `.pane[hidden]` 的 `!important` 被改掉了，或页面 JS 被拦 |
| 操作完跳回概览 | 重定向漏了 `tab=`；各操作应回自己的标签（加人→users、停用模板→templates） |
| 导出列对不上 | 用提交自带的快照取列；若出现串列，说明有记录没存快照（v0.2.0 之后不会发生） |

---

## 8. 实测记录

### 2026-10-28 晚（v0.2.0 表单，`target/release/bks-updates` 2,639,072 字节）

本机起服务（`DATA_DIR=/tmp/bksform`、`ADMIN_TOKEN=secret123`、`BIND=127.0.0.1:18080`、`TZ_OFFSET=480`），
**36 项 curl 冒烟全部通过**：

| 用例 | 结果 |
|---|---|
| 未登录访问 `/admin/forms` | 返回登录页 |
| `/admin/users/save` 加人 | 303 `已保存「张三」` |
| `/form/login` 名单外 | 403 `姓名不在名单里，请联系管理员` |
| `/form/login` 名单内 | 200 `{user_id, name, token, expires_at}`（30 天） |
| `/form/edit-login` 错口令 / 对口令 | 403 `管理口令不对` / 200 `edit_token`（7 天） |
| 无 `edit_token` 存模板 | 403 `需要先输入管理口令` |
| 存模板（5 种字段） | `{"id":1,"version":1}` |
| `select` 不给 options | 422 `下拉字段「A」至少要 2 个选项` |
| 升版 `base_version=1` | `{"id":1,"version":2}` |
| 再用旧的 `base_version=1` | 409 `模板已被别处修改（现在是 v2）`，**库里 version 仍是 2** |
| `/form/templates`（填写页） | 只返回启用中的 |
| `/form/templates?all=1` 无 / 有 edit_token | 403 / 1 条（含停用项） |
| 无 `X-Bks-Form` 提交 | 403 `登录已失效` |
| 正常提交 | `{"status":"created"}` |
| 同一 `submission_id` 再提交 | `{"status":"duplicate"}`，**库里仍只有 1 条** |
| 必填缺失 / 下拉值非法 | 422 + `fields` 逐字段错误 |
| 按旧版本 v1 补交 | `created`，快照是 v1 那份（799 字节 vs v2 的 496） |
| 不存在的版本 | 422 `表单版本已失效，请刷新后重填` |
| 坏 `submission_id` | 400 |
| `/form/mine` | 2 条，带 `client_time` 与 `created_at` |
| 管理页渲染 | 副标题「1 个模板 · 2 人可填 · 共 2 条提交」，内容按字段顺序拼「标签：值」 |
| 模板停用 → 填写页 | `[]`（拿不到了）；编辑器 `?all=1` 仍可见 |
| 停用用户后登录 | 403 |
| 日期筛选 / 空结果 | 「共 1 条提交」/ 「没有符合条件的记录」 |
| 导出未选模板 | 303 回页面并提示「一次只导一个」 |
| 导出选模板 1 | 200，5895 字节，sheet 名 `补发申请`，文件名 `补发申请-2026-10-28.xlsx` |
| 导出内容 | 列 = 并集 `订单号/补发原因/新字段/数量/备注/发生日期` + `提交人/填写时间/接收时间`；缺列留空 |
| 时间列 | `2025-12-06 13:46`（UTC 05:46 +8），`TZ_OFFSET` 生效 |
| 老库（只有 `releases` 表）启动 | 四张 form 表自动建好，`releases` 数据原样保留 |
| `docker build`（本地，国内镜像源） | 1 分 12 秒，镜像 **4.92MB**（比 3.66MB 大，是 xlsx 依赖） |
| 容器内复验 | `--version` 0.2.0；登录 → 加人 → 存模板 → 提交 → 导出 xlsx 全通，内存 3.9MiB |
| `cargo build --release` | 0 warning |
| `cargo test --release` | **12 项全过** |

### 2026-10-28（v0.1.0，`target/release/bks-updates` 1,927,872 字节）

本机起服务（`DATA_DIR=/tmp`、`ADMIN_TOKEN=test123`、`BIND=127.0.0.1:18080`）逐条 curl：

| 用例 | 结果 |
|---|---|
| `GET /health` | 200 `ok` |
| `GET /latest.json`（空库） | 404 `{"error":"还没有发布任何版本"}` |
| `GET /latest.json?client=xx` | 400 `{"error":"未知客户端：xx"}` |
| `POST /admin/login` 错口令 | 401 |
| `POST /admin/login` 对口令 | 303 → `/admin?ok=登录成功`，种下 `bks_admin` Cookie |
| `POST /admin/upload`（client=web, 0.2.0，带 Inno 标记的假包） | 303 `已发布 网页版 v0.2.0（0.20 MB，安装包）` —— 包类型判对 |
| `GET /latest.json?client=web` | 200，`version/notes/package/sha256/size/lan_url/public_url` 齐全 |
| `GET /latest.json`（上传 web 后） | 仍是 404 → 两条线互不影响 |
| `GET /packages/bkstools-0.2.0.exe` | 200，size 一致；下载文件 sha256 与源文件**完全相同** |
| `GET /packages/..%2F..%2Fsecret.key` | 400（白名单拦掉） |
| `GET /packages/secret.key` | 404（名字合法但不在 `packages/`） |
| 上传 python 线 1.1.2（无 Inno 标记） | 303 `已发布 Python 版 v1.1.2（0.20 MB，程序）` —— 判成 portable |
| 两条线同时读 | `python: 1.1.2 portable` / `web: 0.2.0 installer` |
| 上传 web 0.3.0 后 `POST /admin/publish/1` | web 回到 0.2.0，python 仍 1.1.2（未受影响） |
| `POST /admin/delete/1`（删当前版本 0.2.0） | web 自动顶上 0.3.0；`packages/` 里 0.2.0 的文件消失，1.1.2 与 0.3.0 还在 |
| `--version` | `bks-updates 0.1.0` |
| `BIND=127.0.0.1:18080 --health` | 退出码 0（不设 BIND 时去连 8080 → 1，符合设计） |
| `cargo build --release --offline` | 0 warning；`cargo check` 0 error |

#### 镜像构建与容器内验证（同一天稍晚，Docker Desktop 装好后补测）

```bash
docker build --platform linux/amd64 \
  --build-arg RUST_IMAGE=docker.m.daocloud.io/library/rust:1-alpine \
  -t bks-updates:local .            # 1 分 16 秒
```

| 用例 | 结果 |
|---|---|
| 镜像体积 | **3.66MB**（`docker history` 里二进制层 2.35MB，其余是 config/元数据） |
| 容器启动 | `docker run -d -p 18081:8080 -e ADMIN_TOKEN=test123 ...` 立刻 Up |
| `GET /health` | 200 `ok` |
| `GET /latest.json` | 404（空库） |
| 登录 + 上传（假包） | 303 / 303，回显 `已发布 网页版 v0.2.0（0.05 MB，程序）` |
| `GET /latest.json?client=web` | 200，`lan_url` / `public_url` / sha256 / size 齐全 |
| 下载并比对 sha256 | 与上传文件一致（`sort -u` 后只剩一个 hash） |
| `docker exec bks-local /bks-updates --health` | 退出码 0（scratch 无 shell，直接 exec 二进制可行） |
| `docker exec bks-local /bks-updates --version` | `bks-updates 0.1.0` |
| `docker stats` | 内存 **4.49MiB** / 7.7GiB |

**没做**：NAS 实机部署、真机自动更新（客户端侧）。

#### 去掉旧 Python 版之后（同日 15:20 前后，本机 + 容器各验一遍）

| 用例 | 结果 |
|---|---|
| `/latest.json?client=web`（空库） | 404 `还没有发布任何版本` |
| `/latest.json`（不带参数） | 404 `旧版客户端已停止发布，请换用新版` |
| `/latest.json?client=python` | 404 `未知客户端：python` |
| 上传（表单里已没有 client 字段） | 303 `已发布 v0.2.0（0.03 MB，程序）` |
| 清单 | `version=0.2.0`、`client=web`、两个 url 按 `LAN_BASE`/`PUBLIC_BASE` 拼对 |
| 管理页结构 | 没有客户端下拉（`name="client"` 出现 0 次）、没有「两条版本线」提示；表头 版本/大小/下载/发布时间/操作；副标题「当前发布 v0.2.0（N 个版本在册）」 |
| 手工插一条 `client='python'` 且 `is_current=1` 的老记录 | 清单仍返回 0.2.0（漏不出去）；该行显示「旧版·已停发」、**不显示"当前"**、没有"发布"按钮 |
| `POST /admin/publish/<旧版 id>` | 303 → `v1.1.2 是已停发的旧版客户端记录，不能切回` |
| 管理页时间 | 由 UTC 改为本地：版本显示 `10-28 15:20`，与系统时间一致（原来会显示 `07:20`） |
| `cargo build --release` | 0 warning |
| `docker build` + 容器内复验 | 镜像 3.66MB，接口行为与本地一致 |

#### ghcr 上的镜像（14:45，Actions run #2 成功）

| 项 | 值 |
|---|---|
| 地址 | `ghcr.io/wukaibo98/beikeshi-updates:latest` |
| digest | `sha256:8c6ffcfa2c2cc6b4bb079883c1098349ce26b0ca06a8742b57985b640df410e4` |
| 架构 | `linux/amd64` + `linux/arm64`（另有两个 attestation manifest） |
| 体积 | 本机拉 amd64 变体 3.68MB |
| 匿名拉取 | 通过（包是 public，NAS 上不用 `docker login`） |
| 跑起来 | `/health` 200、`--version` → `bks-updates 0.1.0` |

第一次（run #1）编译成功后 push 被拒，原因是旧仓库留下的同名孤儿包；删包后重跑即成功。

### 更早（Python 版，2026-10-25）

上传 13.1MB 安装包 → 清单 `package: "installer"`、sha256 与 `shasum -a 256` 逐字符一致；
旧库迁移到 Rust 版后 python 线正常、web 线 404。

---

## 9. 交接检查清单（动手前逐条打勾）

- [ ] `~/.cargo/bin/cargo build --release --offline` 能过
- [ ] 会起本地服务并用 curl 走一遍「登录 → 上传 → 读清单 → 下载对 sha256」
- [ ] 知道 **`is_current` 必须按 client 收窄**（改错会把另一条线顶下线）
- [ ] 知道 **404 是语义不是故障**
- [ ] 知道页面模板只能用 `%%占位%%`，不能用 `format!`
- [ ] 知道仓库名必须叫 `beikeshi-updates`（镜像名从 `github.repository` 来）
- [ ] 知道本机**有** docker 但 Docker Hub 被拦，构建要带 `--build-arg RUST_IMAGE=...`
- [ ] 知道客户端里的两个更新地址在本仓库之外（`../beikeshi-cs-web`）
- [ ] 提交前 `git status` 确认 `target/`、`data/`、`*.db`、`.env` 没进仓库
- [ ] 口令/密钥不写进仓库；`ADMIN_TOKEN` 走环境变量

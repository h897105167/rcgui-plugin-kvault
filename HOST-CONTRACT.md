# 和主程序之间的接口契约

这份文件是插件侧对主程序提出的要求：哪些已经能对上、哪些必须主程序改、哪些是插件无论如何做不到的。写这份文件的原因是插件只能通过宿主给的 8 组能力说话，通道里没有的东西插件补不出来；与其在插件里假装能发文件，不如把边界写清楚，让两边按同一张表验收。

每一条都写明了：现状、要什么、怎么算做到了。行号指主程序仓库（`rclone_gui_windows`）当时的状态，改动后会对不上，按函数名找。

先跑这一条，看主程序里现在有多少处直接写着插件 id——H3 要消掉的就是它们：

```bash
git -C ../rclone_gui_windows grep -n kvault -- crates/app crates/plugin
```

插件这边能跑起来的样子（65 个测试，其中 6 个真的开了本机 socket 收发请求）：

```bash
cd plugin_repos/rcgui-plugin-kvault && cargo test --offline
```

## 一、分工

| 事情 | 谁做 | 为什么 |
| --- | --- | --- |
| 站点地址、令牌、目录、存储、去重这几项配置 | 插件 | 这些是远程参数，插件建远程时写进 rclone；编辑它们的表单也该由插件面板描述，不是主程序写死的一块界面（见 H7） |
| 扫描旧远程、记清单、填表单、导出恢复 | 插件 | 只读 rclone 配置、只写自己的 kv |
| 列目录 | 插件 | 插件自己发 `GET /api/v1/files`，地址、认证头、分页字段、错误文案全在插件里（`src/api.rs`） |
| 删除文件 | 插件 | 插件自己发 `DELETE /api/v1/file/<id>`，删前弹一次确认 |
| 上传要发到哪、带什么头、表单放哪些字段 | 插件 | 这是站点知识，主程序里不许有 |
| 上传时把文件字节搬出去 | 主程序 | 插件的 `http.request` 只收字符串 body，字节搬运必须由宿主做，见 H8 |
| 短链 | 插件 | 上传响应里的 `links.download`，插件拼成完整地址再显示 |
| 「这个远程算不算 K-Vault」的显示判定 | 主程序 | 只有主程序知道插件启没启用（见 H2） |
| 令牌谁先谁后 | 主程序定，插件跟 | 主程序上传前解析令牌，顺序必须一致（见 H5） |

一句话：主程序只提供「把某个用户选中的文件按插件说的方式发出去」这一件通用的事，其余全是插件的事。

## 二、已经对得上的部分

这几件事不用改，插件已经在用，列在这里是为了让宿主侧的人知道有谁依赖它。

| 插件用的能力 | 宿主实现位置 | 返回形状 |
| --- | --- | --- |
| `kv.get_all` | `crates/app/src/kv.rs`（`KvStore::get_all`），每个插件一个 `<data>/plugins/<id>/kv.json` | `{key: value}` |
| `rclone.remotes` | `crates/app/src/caps.rs`（`rclone_remotes`） | **要注意**：文档写的是裸数组 `["a","b"]`，实现返回 `{"remotes":[…],"source":"rc"\|"conf"}`。插件两种都认，所以不用改；但文档和实现对不上，建议宿主择一 |
| `rclone.call` | `crates/app/src/caps.rs`（`rclone_call`） | 原样转发 RC 的返回；`config/get` 是扁平参数，没有 `params` 外壳 |
| `rclone.call` 的写操作拦截 | `crates/app/src/caps.rs`（`kv_write_op`） | `operations/delete`、`operations/purge` 对 K-Vault 远程不走 rclone，改成「读站点索引 → 删 id」；`movefile/move/copyfile/copy/mkdir/rmdir` 直接报「DAV 只读」。**H8 之后这段应当整块消失**：插件自己有删除动作，主程序不必再替它翻译 |
| `ui.copy`、`ui.confirm`、`ui.toast` | 同文件 | `ui.confirm` 回 `{"ok": <bool>}`，插件删文件前用它；关掉对话框返回 `false`，插件按「不删」处理 |
| `remote_types` 派生类型 | `crates/app/src/remote_types.rs`（`collect_in`、`Derived::initial_params`） | 只收启用插件；`type` 最后写入，插件改不了后端——这一条正是「类型只从 rclone core 和启用的插件来」的落点，已经是对的 |

## 三、已经定死、插件按它实现的行为

- `enabled: false` 只是把按钮灰掉，宿主不会拦动作。所以「没有记录名就去点填表单」这类错误必须插件自己报，插件确实在 handler 里拦了。
- 一个动作最多 16 轮（上限 32），`state` 是唯一能跨轮带数据的地方。扫描因此做成批量 `need`：一次最多问 32 条远程的参数，与远程个数无关，4 轮结束。列表也是这个道理：一页一轮，最多 5 轮。
- `ops` 的返回值会被丢掉，需要结果的一律走 `need`。所以「导出」用 `need` 取 `ui.copy` 的结果，复制失败用户能看见。
- 能力调用失败不中断整轮，返回 `{"_error": …}`。插件据此把「某条远程读不出来」降级成状态行里的一句备注，不让整次扫描失败；`http.upload` 不存在时也是这样变成一句「宿主还没有这个能力」。

## 四、为什么字节必须由主程序搬

这一节只讲一条边界，H8 是它对应的接口。

插件发请求的路是 `http.request`，它的 `body` 是字符串。multipart 的文件段要求把文件内容原样接在一串头后面，几百 MB 的二进制没法先变成一个 JSON 字符串——就算变成了，通道也会被这一轮撑爆。插件也不申请 `fs`（读用户磁盘）和 `net`（自己开 socket），这两条是刻意的：一旦给了，插件就能读任意文件、连任意地址，而「只发用户这次选中的那一个文件」这件事就没人保证了。

所以插件做的是把请求**描述**清楚：地址、方法、头、表单字段、文件字段名、上限、超时。这一份描述交给宿主，由宿主打开用户选中的那个文件，按描述发出去。这就是 H8。

插件**不能**自己发大文件，这一点没有绕过的办法；任何声称插件能直接上传的说法都是错的。

## 五、需要主程序改的地方

### H1 把旧版明文令牌表挪出插件自己的命名空间

**现状**：内建版以 `kvault` 为命名空间写 `<data>/plugins/kvault/kv.json`（`crates/app/src/kvault.rs` 的 `STORE_NS = "kvault"`），而插件的 id 也是 `kvault`，宿主的插件 kv 正好是同一个文件（`crates/app/src/kv.rs` 的 `file_of`：`dir.join(plugin_id).join("kv.json")`）。好处是装完插件旧设置自动可见；坏处是宿主的「干净卸载」会删掉整个插件目录，也就是删掉这份**唯一的明文令牌**。插件已经加了导出弥补，但正确的做法是把两份数据分开。

**要什么**：内建版的令牌表换到独立命名空间（比如 `kvault_tokens`），`kv_token_resolve` 先读新位置，读不到再读旧位置（保留一个版本的兼容）。

**验收**：卸载插件后 `<data>/plugins/kvault/` 消失，令牌表仍在；重新装回来，上传照旧能用。

### H2 「这个远程是不是 K-Vault」的显示判定要看插件启没启用

**现状**：一共四处，源头是列表那一处，另外三处跟着它走。

1. `crates/app/src/refresh.rs:96-101`：远程列表把 `type = webdav` 且带 `kvault = 1` 的远程改名成 `k-vault` 显示，不看插件状态。
2. `crates/app/src/main.rs:3158-3165` `edit_single_choice`：编辑一条已有远程时，类型只要是 `k-vault` 就合成一项 `K-Vault` 塞进下拉框（注释写的是为了兼容老用户的类型名），同样不看插件状态。
3. `crates/app/src/main.rs:3232` `update_editor_fields`：类型名等于 `k-vault` 就清空通用参数表、改显示写死的专用表单。
4. `crates/app/ui/main.slint:2987`、`3072`、`3161`、`3187`：专用表单本身，以及它顺带隐藏的通用参数表、保存按钮和原始参数框（`RemotesG.kv-editor` 见 `main.slint:507`）。

新建入口已经是对的：`crates/app/src/main.rs:3114` `build_type_choices` 只列内核报的后端加已启用插件的派生类型，内置的 `k-vault` 早被删掉了，注释里记着「用户明确要求类型只来自 rclone」。缺的是上面四处。

**要什么**：四处都以「这个类型由某个已启用插件的 `remote_types` 认领」为前提，判断方式是从 `crates/app/src/remote_types.rs` 的 `collect_in` 拿到的派生列表里查有没有这一项，而不是在源码里比对 `"k-vault"` 这个字符串（铁律 §7.1）。插件不启用时，这条远程就是个普通 `webdav`：列表显示 `webdav`，编辑它看到通用参数表。

**验收**：禁用插件重启 → 列表里显示 WebDAV、编辑它看到 webdav 的参数表、类型下拉里没有 `K-Vault`；启用后恢复。宿主侧加一条测试覆盖两态。

### H3 上传与删除都不许按字符串认 K-Vault

**现状**：能力层里有两段按 K-Vault 分流的硬编码逻辑，都在 `crates/app/src/caps.rs`：

- `kv_write_op`（`caps.rs:283-350`）档在每个 `rclone.call` 前面：`operations/delete`、`operations/purge` 对 K-Vault 远程不发给 rclone，改成「读站点索引 → 按目录和文件名挑 id → 删 id」；`operations/movefile|move|copyfile|copy|mkdir|rmdir` 直接报「站点的 DAV 只读」。这段判定靠的是「这个远程能不能被 `kv_cfg_lookup` 认成 K-Vault」，而 `kv_cfg_lookup` 靠 `kvault = 1` 标记加固定类型常量。
- 上传分支 `rclone_upload`：普通远程走 `operations/copyfile`，K-Vault 走站点 `POST /api/v1/upload`。

另有 `crates/app/src/main.rs:3294` 的 `KVAULT_PLUGIN_ID` 常量，注释自己承认「它和 `kvault::STORE_NS` 现在同一个字符串，但含义不同」——这是主程序源码里的插件 id，铁律 §7.1 不允许。

**要什么**：H8 落地之后，这两段都不必存在——列目录、删除、上传都是插件自己的动作，主程序不必知道任何一个站点接口。剩下的部分（如果还想给 `rclone.copyto` 之类的调用兜底）改成「这个类型的 id 是否出现在已启用插件的 `remote_types` 里」，或者按派生项自带的 `base_type` + 参数标记判断，源码里不出现插件 id 字符串。`KVAULT_PLUGIN_ID` 换成从启用插件列表里取。

**验收**：把插件目录整个删掉，主程序仍能编译、普通远程的上传和删除不受影响；`git grep -n kvault -- crates/app/src crates/app/ui crates/plugin/src` 只剩测试数据与 `crates/app/src/kvault.rs` 自身（那份实现要等 H7 之后才能整块拿掉）。

### H4 卸载前的提醒

**现状**：卸载会连 kv 一起删，而插件在卸载时已经不可调用，没法自己提醒。

**要什么**：宿主在卸载确认框里说明「这个插件的设置会被一并删除」，或者在删之前把 `kv.json` 备份到 `<data>/plugins/_removed/kvault/`。插件侧的导出是给用户主动用的，不能替代这一步。

**验收**：卸载 → 目录里留有备份（或用户明确看过提示）→ 重装后设置还在。

### H5 令牌顺序双向一致

**现状**：主程序解析令牌的顺序是「按远程名存的明文令牌 → 远程参数里的 `bearer_token`（`config/reveal` 还原）→ `site` 里的令牌」（`crates/app/src/main.rs` 的 `kv_token_resolve`）。插件按同一顺序展示「令牌已设置/未设置」，`token_for` 就是这三步。

**要什么**：任何一方改顺序都要同时改另一边，否则会出现面板说有令牌、上传说没凭据。

**验收**：只保留第三处令牌时，面板和上传都认。

### H6 `rclone` 这组能力的权限粒度

**现状**：`rclone.call` 没有方法白名单，拿到 `rclone` 能力就等于拿到整个 RC 接口，包括 `config/delete`、`operations/delete`。插件只需要 `config/get`、`config/reveal`、`config/providers`、`config/create`、`config/listremotes` 五个方法，迁移过程中一个写操作都不发，列目录/删除走站点接口也不经过 rclone。

**要什么**：宿主给 `rclone.call` 加方法白名单（或把「写 rclone 配置」单列成一个能力组）。这是宿主的安全决定，插件侧只能声明自己用哪些方法——README 的权限表里已经把这点写明了。

**验收**：一个只申请了只读方法的插件发 `config/delete` 被拒，拒绝文案和其它能力一致。

### H7 把写死的 K-Vault 表单从界面里拿掉

**现状**：`crates/app/ui/main.slint:2979-3026` 有一整块专用表单（站点地址、API 令牌、上传目录、存储位置），配套属性 `kv-editor`、`kv-token-set`（`main.slint:507`、`513`），值由 `crates/app/src/main.rs:3467` 的 `apply_kv_form` 灌进去，保存走 `crates/app/src/main.rs:8549-8577` 那条分支。铁律 §7.1 明确不许有插件专属 Slint UI，而插件已经有声明式面板（`config.json` 的 `plugin_setting`）描述这些字段，包括「站点上的文件」那一页。

**要什么**：这几个字段归插件的面板去描述，主程序只保留通用参数表和面板容器。注意别一起删掉 `kv_token_resolve`：上传时解析令牌是宿主职责，H5 的顺序还得靠它。

**验收**：`main.slint` 里搜不到 `kvault` / `k-vault`；编辑一条 K-Vault 远程时，站点地址、令牌、上传目录出现在插件面板里，改完保存后上传用的是新值。

### H8 新增一个通用能力 `http.upload`：把用户选中的那一个文件流出去

**现状**：`http.request` 的 `body` 只收字符串（`crates/app/src/caps.rs` 的 `http_request`），插件拼不出 multipart 的文件段。于是上传、列目录、删除都被塞进了主程序（`rclone_upload` 的 K-Vault 分支、`kv_write_op`），K-Vault 的站点知识因此散落在宿主里——这正是要拆掉的东西。

**要什么**：一个只管字节搬运的通用能力。插件说往哪发、带什么头、表单里放哪些字段；宿主负责打开**用户在这次动作里亲手选中的那一个文件**，按描述发出去。宿主不需要认识 K-Vault，也不需要认识任何站点地址或字段名——这些全在插件里，换一个插件用同一个能力上传到别处，宿主代码不用改。

插件发来的请求（`need` 的 `params`）：

```json
{
  "url": "https://<站点>/api/v1/upload",
  "method": "POST",
  "headers": { "Authorization": "Bearer …" },
  "source": { "form_key": "file" },
  "file": { "field": "file", "name": "a.png", "mime": "image/png" },
  "fields": [
    { "name": "storage", "value": "r2" },
    { "name": "folderPath", "value": "album" }
  ],
  "idem_seed": "kvault-0123456789abcdef",
  "max_file_bytes": 104857600,
  "timeout_ms": 600000,
  "max_bytes": 65536
}
```

两个上限名字必须分清，这是宿主最容易看错的地方：`max_file_bytes` 是「这个文件最多多大」，`max_bytes` 与现有 `http.request` 同名同义，是「回包最多收多少就截断」。不要用一个字段兼管两件事。

宿主的回复：

```json
{ "status": 200, "body": "…", "size": 123456 }
```

`size` 是真正发出去的文件字节数。失败时按既有约定回 `{"_error": "…"}`，插件据此显示一句人话：

| `_error` | 什么时候 |
| --- | --- |
| `SOURCE_NOT_GRANTED: file` | 这次动作里用户没选过这个字段的文件，或插件给的字段名不在表单里 |
| `FILE_TOO_LARGE: 157286400 > 104857600` | 文件超过 `max_file_bytes`，请求没发出去 |
| `HOST_HEADER: Content-Type` | 插件的 `headers` 里出现了该由宿主决定的头 |
| `CANCELLED` | 用户中途关掉面板或切走了，流被中断 |

宿主必须守住的规则，也就是这个能力的权限边界：

1. `source.form_key` 只能在宿主自己的授权表里查：它必须是本次动作的表单里、**宿主自己那一行「浏览…」按钮**选出来的路径。插件送来的任何路径字段（`file.path`、`source.path`、`path`）一概不看，出现了就直接 `SOURCE_NOT_GRANTED` 拒掉。这样插件就没有办法借这个能力读走任意本地文件；它连文件叫什么都只能从面板回填的值里知道。
2. 只打开那一个文件，发完就关。不许再按任何其它字符串打开文件或目录。
3. 发之前先看长度：`> max_file_bytes` 直接 `FILE_TOO_LARGE`，不要发出去。站点那边超限回 413，先挡一次省下一次白传。
4. `Idempotency-Key: {idem_seed}-{文件内容摘要}`。摘要必须由宿主算：插件拿不到文件，算不出内容摘要；如果让插件自己算 key，同一个文件重试时 key 会变，站点就会存两份。摘要算法由宿主定（sha256 取前 16 字节就够），插件只保证同一目的地、同一文件名的 `idem_seed` 不变。
5. `multipart/form-data` 的 boundary、`Content-Type`、`Content-Length` 由宿主决定；插件 `headers` 里出现 `Content-Type`、`Content-Length`、`Host`、`Connection` 一律 `HOST_HEADER` 拒绝，否则插件能悄悄换掉内容类型。
6. `fields` 按数组顺序写在文件段**之前**——有的服务端框架只读文件段前面的字段。
7. 取消：用户关面板、切页、退出，宿主中断 socket 并回 `CANCELLED`。已经发出去的字节收不回来，插件如实显示「不确定是否传完，列表里看一眼」。
8. 回复的 `body` 超过 `max_bytes` 就截断，与 `http.request` 的做法一致；几百 KB 的 HTML 错误页不该整份塞回通道。
9. 这个能力加在 `http` 组里，不新增能力组；插件仍然不申请 `fs` 和 `net`。
10. 同一份 `http.upload` 不许并发：一次动作一个文件，够用，也让「只有一个文件被授权」这件事好验证。

**验收**：宿主侧加两条测试——(a) 插件用 `source.form_key` 指到一个用户没选过的字段，被拒且没有任何请求发出；(b) 用户选中的文件被完整发到一个本机假服务端，字节数一致、`fields` 出现在文件段之前、`Idempotency-Key` 以插件给的 `idem_seed` 开头。插件的真实 socket 测试（`src/wire.rs`）已经按这个形状实现了一遍宿主该做的事，可以直接照着写。

## 六、插件申请的权限

`config.json` 里只有四组，加一条元数据：

| 能力 | 用来做什么 | 主机侧风险 |
| --- | --- | --- |
| `kv` | 站点设置、清单、每远程明文令牌 | 只读写自己那个文件 |
| `ui` | 复制 DAV 地址、复制导出内容、删文件前确认 | 只弹自己说明的对话框 |
| `http` | 打站点 `/api/health`、`/api/v1/me`、`GET /api/v1/files`、`DELETE /api/v1/file/<id>`；上传时交出一份请求描述 | 只连用户填的地址。插件自己不发文件字节（见 H8），列目录与删除的请求都在插件的 `src/api.rs` 里 |
| `rclone` | 列远程、读参数、解密令牌、查后端、建远程 | 见 H6 |
| `remote_types`（元数据） | 让下拉里出现 K-Vault | 派生类型，改不了后端 |

没有 `fs`、`proc`、`net`。

## 七、验收清单

宿主侧改完 H1–H8 之后，用这几步确认整件事成立：

1. 禁用插件 → 重启主程序 → 远程列表里没有 `k-vault` 类型，旧的 K-Vault 远程按 WebDAV 显示；编辑它看到的是 webdav 的通用参数表。
2. 启用插件 → 面板出现五页 → 「扫描旧设置」在 4 轮内返回，状态行里是「找到 N 个」。
3. 扫描完看 rclone 配置文件的修改时间：没变。再看 `config/update`、`config/create`、`config/delete` 的调用记录：没有。
4. 挑一条记录填表单 → 建新远程 → 站点地址、令牌、上传目录出现在插件面板里（不是主程序写死的表单）→ 保存。
5. 「站点上的文件」页：点「列出文件」→ 看见站点上的文件；填一个 id 点「删除」→ 先弹确认，取消则什么都不发，确认后才发 DELETE。
6. 用「浏览…」选一个文件点「上传」→ 站点上出现它，面板显示大小和下载地址。
7. 导出（不勾令牌）→ 文本框里搜不到令牌；勾上再导 → 搜得到。恢复到一个空环境 → 清单和令牌都回来。
8. 卸载插件 → H4 的提示出现、备份存在 → 重装 → 设置还在。
9. 把 `plugins/kvault/` 整个删掉 → 主程序照样编译、普通远程的浏览/上传/删除不受影响，也没有任何东西被当成 K-Vault。
10. 让插件尝试上传一个用户没选的文件（测试里直接伪造 `source.form_key`）→ 被拒，请求没发出。

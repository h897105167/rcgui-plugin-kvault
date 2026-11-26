# rcgui-plugin-kvault

RCGUI 的插件，用来把一台 K-Vault 站点配成 rclone 远程：填站点地址和访问令牌，测通，建出一个 `webdav` 类型的远程，带上 K-Vault 需要的几个参数。上传本身仍由主程序负责，插件不碰文件。

## 它建出来的是什么

rclone 里没有 `kvault` 这个后端。K-Vault 站点讲的就是 WebDAV，另外多认几个参数，所以远程的类型是 `webdav`：

```
[k-vault]
type = webdav
url = https://kv.example.com/dav
vendor = other
kvault = 1
kvault_folder =
kvault_storage = telegram
kvault_dedup = 1
bearer_token = kvault_xxx_yyy
```

插件做三件事：把地址补成 `/dav`、带着令牌打两个探测接口、调 `config/create` 把上面这份参数写进 rclone。它不会凭空造出一个新后端。

## 装载

0.1.0 是首个版本，**还没有发布**：没有 tag，Release 里也没有这个 zip。下面这条是发版之后的路径，现在想试就先在本地打包（见「构建」）。

1. 从 Release 下 `kvault-0.1.0.zip`（正文里有这个 zip 的 sha256，可自行核对）。
2. 解到 `<RCGUI 数据目录>/plugins/kvault/`，目录里应当有 `config.json`、`main.wasm`、`check.sha256`。
3. 在插件页启用。启用后「新建远程」的类型下拉里才会多出 K-Vault 这一项。

宿主需要 0.3.0 及以上（`config.json` 的 `min_gui_version` 写的就是这个）：从这一版起，插件才能声明 `remote_types` 元数据、也才能用面板分页。旧宿主会忽略 `remote_types`，插件能装、能打开，但下拉里不会出现 K-Vault。

## 第一次用

1. 「连接」页填站点根地址，比如 `https://kv.example.com`，不要自己加 `/dav`。
2. 填令牌（`kvault_` 开头）。留空也行，那样只能用账号密码走 DAV。
3. 点「测试连接」。插件先打 `/api/health`，有令牌再打 `/api/v1/me`，把站点名、权限范围、支持的存储、单文件上限念给你听。
4. 「远程」页确认远程名（默认 `k-vault`）、存放目录、存储后端、是否去重，点「创建远程」。
5. 建完 `k-vault:` 就是一个普通 rclone 远程，主程序里照常用。

测试连接不会写任何东西到站点；建远程只写 rclone 配置，不连站点。

## 令牌存在哪儿

建远程时插件写两处：

- 插件自己那一格 `site`：站点地址、令牌、目录、存储、去重，重开面板要用。
- 远程名那一格，值就是 `{"token": "…"}`：主程序上传前按这个顺序找令牌——先按远程名找这一格，再读 rclone 远程参数里的 `bearer_token`（要走 `config/reveal` 还原），最后才看插件那一格。

第二处是为了让**主程序原来的上传路径继续用**：它在这些插件出现之前就按「远程名 → 明文令牌」存了一份，先查这里。所以用插件建的远程，主程序上传时不需要额外配置；反过来，早先用主程序建好的 K-Vault 远程，装了插件也不会被改写。两处都在同一个文件（`<数据目录>/plugins/kvault/kv.json`），令牌在此之前就是明文，插件没有让它更容易读。

## 面板分页

| 页 | 里面有什么 |
| --- | --- |
| 连接 | 站点地址、令牌、说明、测试连接 |
| 远程 | 远程名、存放目录、存储后端、去重、创建远程 |
| 关于 | DAV 地址、复制按钮、上传为何不在插件里 |

「当前状态」和「检查项」两块不挂在某一页上，切页时一直可见。切页不会触发插件动作，表单里改了一半的值也不会丢。

## 它不做什么

它不传文件。站点的上传接口要求把文件本身放进请求体发出去，而宿主给插件的请求通道只收文本，所以上传、列目录、删除仍然走主程序——远程参数里那几个 `kvault_` 开头的东西就是给主程序看的。

它也不支持只有账号密码、没有令牌的站点：插件写的远程参数里只有 `bearer_token`，没有 `user`/`pass`。这类站点在主程序里照旧能用，只是别指望插件替你配。

还有两件它不肯做的事。建的远程能不能用取决于你连着的 rclone 里有没有 `webdav` 后端，没有就停下来说明，不会硬写一个跑不了的远程；令牌以明文存在本机插件目录（和其它插件一样，没有系统密钥环可用）。

## 权限

`config.json` 里只申请这些：

| 能力 | 用来做什么 |
| --- | --- |
| `kv` | 存站点地址、令牌、远程参数 |
| `ui` | 复制 DAV 地址到剪贴板 |
| `http` | 打站点的 `/api/health`、`/api/v1/me` |
| `rclone` | 查后端列表、创建远程 |
| `remote_types` | 让主程序在类型下拉里显示 K-Vault |

没有 `fs`、`proc`、`net`：插件不读文件、不起进程、不开端口。

## 开发

powershell.exe 不行：Windows PowerShell 5.1 会按 ANSI 读这个带中文的脚本，报 `The string is missing the terminator`。用 PowerShell 7 的 `pwsh`：

```bash
cargo fmt --all --check
cargo test
cargo check --target wasm32-wasip1
pwsh -File scripts/pack.ps1            # Windows
bash scripts/pack.sh                   # Linux / macOS / CI
```

`cargo test` 里有两条防漂移测试：存储下拉的选项必须和代码里的清单一致（一边改了另一边没跟上就会红），以及远程参数永远不写空令牌（写了会把 rclone 里已存的令牌擦掉）。

产物在 `dist/kvault-0.1.0.zip`，打包时顺带算出它的 sha256。

## 文件

| 文件 | 作用 |
| --- | --- |
| `src/main.rs` | 动作分发、面板状态 |
| `src/site.rs` | 地址拼装、`/me` 解析、rclone 参数生成 |
| `src/proto.rs` | 宿主协议：`need` / `done` / `fail` |
| `src/i18n.rs`、`src/i18n.json` | 中英文案 |
| `MIGRATION.md` | 从主程序内建 K-Vault 迁过来要注意什么 |

## 许可

MIT。

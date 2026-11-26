# 从主程序内建的 K-Vault 迁到本插件

写给已经在用主程序里那套 K-Vault 的人：插件把「配置远程」这件事接管了，但**上传那半条路仍在主程序里**。看完这一页再决定要不要换。

## 两边各存什么

| 项目 | 主程序内建 | 本插件 |
| --- | --- | --- |
| 存在哪 | `<data>/plugins/kvault/kv.json`，命名空间 `kvault` | 插件自己的 kv，键 `site` |
| 站点地址 | `base`（不含 `/dav`） | `base` |
| 令牌 | `token`（`kvault_<id>_<secret>`） | `token` |
| 账号密码（仅 DAV 用） | `user`、`pass` | 没有 |
| 上传目录 | `folder` | `folder` |
| 存储后端 | `storage` | `storage` |
| 去重 | `dedup` | `dedup` |
| 远程名 | 由主程序管理 | `name`（默认 `k-vault`） |

写进 rclone 的远程参数是同一套，所以插件建的远程和主程序建的远程在 `rclone config show` 里长得一样：

```
type = webdav
url = <base>/dav
vendor = other
kvault = 1
kvault_folder = <folder>
kvault_storage = <storage>
kvault_dedup = 1 或 0
bearer_token = <token>
```

主程序额外还会写 `user` / `pass`（账号密码走 DAV 的那种站点）。插件不写这两项，**只有账号密码、没有令牌的站点请不要用插件配**，继续用主程序。

## 迁移步骤

1. 先用 `rclone config show <你的远程名>` 把旧参数抄下来（令牌在输出里是 obscured 的，抄不到明文，无所谓，下一步重新填）。
2. 装上插件、在「连接」页填站点地址和令牌，点「测试连接」。状态行里应当念出站点名和存储列表；报错就先把地址和令牌弄对，别急着建远程。
3. 到「远程」页填远程名。**想沿用旧名字就在建之前先删掉旧的**：同名远程怎么处理取决于你装的 rclone 版本，插件不替你判断。
4. 点「创建远程」。成功后这个远程就是普通 rclone 远程，主程序里照常用。
5. 旧的 `kv.json` 不用删。插件不读它，主程序如果还在用，删了反而会丢配置。

## 什么搬不过去，需要主程序继续配合

远程参数只是「rclone 怎么连站点」。K-Vault 站点的上传接口不是 WebDAV，它有自己的 HTTP API，这部分插件做不了——宿主的 `http.request` 只能发字符串请求体，发不了 multipart 文件。所以下面这些仍然只能在主程序里实现：

| 功能 | 站点接口 | 为什么插件做不了 |
| --- | --- | --- |
| 上传 | `POST /api/v1/upload`，multipart 文件体，带 `Idempotency-Key` | `http.request` 的 `body` 只收字符串 |
| 列目录 | `GET /api/v1/files`，一次 200 条、翻 25 页 | 同上，而且需要翻页重试的循环 |
| 删除 | `DELETE /api/v1/file/<encode_id(id)>` | 请求得带认证头，能做，但没有列表就拿不到 id |
| 直链 | 上传回包里的 `links.download` | 依赖上传结果 |
| 去重判定 | 站点按 `deduplicate` 与文件大小决定 | 依赖上传结果 |
| 单文件上限 | 100 MB，超了站点直接 413 | 依赖上传结果 |
| 账号密码站点 | DAV basic auth | 插件不写 `user`/`pass` |

一句话：插件只接管「建远程、验令牌」，靠远程参数复现不了旧的上传路径。上传、列目录、删除这几步要么继续留在主程序里，要么由核心会话重写一遍；迁移时别把主程序里那段删掉。

## 令牌为什么是明文

rclone 把 `bearer_token` 存成 obscured，`config/dump` 只能读到那一串，老版本 rclone 也没有 `config/reveal`。上传接口要的是明文令牌，所以主程序另外留了一份在 `kv.json`。插件同理，令牌明文存在本机插件目录里——这是已知的债，不是这次的改动范围。

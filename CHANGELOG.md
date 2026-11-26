# 更新记录

## [Unreleased] 0.1.0

首个版本，尚未发布。把主程序里内建的 K-Vault 配置搬到插件里，界面分三页；上传仍然由主程序内核完成，插件不碰文件。

**需要的主程序版本**

- 主程序 0.3.0 或更高。`remote_types`（让「新建远程」的类型下拉里出现 K-Vault）和面板分页都从这个版本起才有；更老的宿主会忽略这两项，插件只剩面板。
- 能力组只要 `kv`、`ui`、`http`、`rclone`。`remote_types` 是插件元数据，不是能力组——把它写进 `capabilities` 会被宿主当成未知能力组拒绝。

**连接页**

- 填站点根地址与 `kvault_` 令牌，「测试连接」打一次 `/api/health` 和 `/api/v1/me`，把站点名、权限范围、可用存储、单文件上限写在状态行里。
- 令牌写两处：一处是插件自己 kv 里按远程名存的 `token`，一处是建远程时给 rclone 的参数。前者供主程序旧版内核读取——旧版从 `__kvault` 命名空间取令牌，与插件用的是同一个 `kv.json`，所以两种读法都能取到；后者供 rclone 直接用。远程参数永远不写空 `bearer_token`：写空会把 rclone 里已存的令牌擦掉。

**远程页**

- 远程名、存放目录、存储后端（telegram / r2 / s3 / discord / huggingface / webdav / github）、是否去重，一键建出 `webdav` 类型的远程。
- 建远程前先查 `config/providers`：连着的 rclone 没有 `webdav` 后端就停下来说清楚，不硬写一个跑不了的远程。
- 写进 rclone 的 `type` 恒为 `webdav`，K-Vault 只是派生类型；`vendor`、`kvault`、`kvault_storage`、`kvault_dedup` 随 `defaults` 一起给过去。

**关于页**

- 显示并复制 DAV 地址（`<站点>/dav`），并说明为什么上传不在这里做：上传要发 multipart 和流式读取，宿主的能力接口只接受字符串请求体，搬进插件就得让插件自己开网络连接，那既超出插件权限，也绕过了内核已有的去重、索引与短链逻辑。

**工程**

- 14 个测试用假宿主驱动真实的 `dispatch`：两处令牌都写、没令牌时不碰 rclone、建远程失败如实报错、`config.json` 的元数据形状、存储下拉与代码清单一致、远程参数不写空令牌。
- 中英文案同集对齐，占位符编号一致；`.github/workflows/ci.yml` 跑 fmt、`cargo test`、`cargo check --target wasm32-wasip1`、`config.json` 结构校验与打包校验（zip 恰为 `check.sha256`/`config.json`/wasm 入口，清单逐行核对）。
- `release.yml` 打包前交叉校验 tag / `config.json` / `Cargo.toml` / `Cargo.lock` 四处版本并跑格式、测试、wasm 编译；只在 tag 时发布，同名 Release 已存在就失败退出，不覆盖已发布资产的 sha256；手动运行只产 artifact。
- 打包脚本给编译加了 `--remap-path-prefix`：发布的 wasm 里不再出现本机的 cargo 路径。

# 来源、署名与许可范围

项目：`natpierce-kernelsu`。作者与维护者：**Chimiko**。

## 本仓库开源部分

`src/` 的 Rust 管理服务、Android 网络辅助逻辑、模块脚本与 `module/webroot/` 的 Material Design WebUI 为本项目重新实现，采用根目录 [LICENSE](LICENSE) 中的 MIT License。运行不依赖早期原型的 Go 管理服务。

多个 AI 助手先后参与方案探索、核心兼容研究、Rust 实现、WebUI、审查与真机验证。早期 AI 原型提供失败现象和视觉方向的参考，旧原型及其研究文件不作为本仓库发布内容。AI 参与不改变第三方软件的许可归属。

## 皎月连官方核心

登录、P2P/NAT 穿透、组网与实际隧道协议由原始官方 Linux ARM64 核心执行。本次验证与模块内置回退来源为 v1.06；后续版本通过独立清单提供。本项目没有声称这些私有协议已完整重写为 Rust，也没有修改核心指令或关闭官方 WebSocket 并发检测。

- 官网：[https://www.natpierce.cn/](https://www.natpierce.cn/)
- 官方下载：[natpierce-arm64-v1.06.tar.gz](https://download.natpierce.cn/linux/natpierce-arm64-v1.06.tar.gz)
- 解压后 `natpierce` 可执行文件的 SHA256：`7edafed7d1093c27c8ce20458e72ce060243c2204ecdba23aeb6e17ea89cef78`

官方二进制、官方 Android APK、官方 Windows 软件、商标与其他相关内容不适用本仓库的 MIT License。公开提供下载不等于采用开源许可证，相关权利属于原软件提供方，使用者应遵循官方条款。

**GitHub 仓库、源码包和 Release 模块不分发官方核心。** 使用者在 WebUI 中主动点击后，模块从官方 HTTPS 地址下载并校验原始核心。下载后的核心保存在使用者手机的 root 私有目录中。上述内置哈希及版本清单中的哈希均对应解压后的可执行文件，不对应压缩包。

[core-manifest.json](core-manifest.json) 只分发版本元数据，包含维护者核验过的版本、对应官方下载地址和 SHA256。检查清单与安装更新由使用者分别主动触发；维护者更新推荐版本无需重新分发模块。版本清单不包含官方二进制，也不改变其许可归属。

本项目为非官方 KernelSU 集成，与皎月连提供方没有隶属关系或官方背书。

## 第三方依赖

Rust 依赖及其精确版本记录在 [Cargo.toml](Cargo.toml) 和 [Cargo.lock](Cargo.lock)。完整许可文本与上游署名见 [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md)，发布的 Rust 二进制应附带该文件。

大多数依赖允许 MIT 或 Apache-2.0 二选一；`encoding_rs` 另含 BSD-3-Clause 许可内容，`unicode-ident` 另含 Unicode-3.0 数据许可。根目录 MIT License 不替代这些附加义务。

Material Design 描述的是界面风格；WebUI 不加载 Google 字体、官方 Material 组件或外部脚本。模块 WebUI 通过 KernelSU/兼容管理器提供的桥调用 root 命令，不分发管理器或内核组件。

## 公开内容与私有数据

发布使用明确文件白名单，不包含测试账号、密码、令牌、抓包、手机数据、反汇编资料、旧项目、官方软件、编译器或依赖缓存。公开截图来自真机页面，并对账号、识别码及设备名称等私有内容进行脱敏。

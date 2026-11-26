# iPhone USB 导入故障与开源项目对照

2026-11-25。范围为 Windows Time-cut 真机记录、公开源码和官方技术资料。外部项目在同一台 Windows 电脑和同一部 iPhone 上的导入结果尚未验证。

## Time-cut 的故障

**最可能的原因是 iPhone 经 Windows 导出的文件信息与实际读到的文件版本不一致。** Windows 便携设备接口（WPD）同时影响 Time-cut 的取件顺序和收齐判定，因而少数异常文件可以拖住已收到的一批原件。系统在哪一层生成了不一致的长度，仍需受控对照。

- 第一部 iPhone 已配对，Apple 文件服务（AFC）打开后却无法列出 /DCIM，独立客户端返回 NO_RESOURCES。Windows 同时可见 66 个直接展示的月份目录，并完整读出一段视频。preview.40 漏扫了没有显式 DCIM 父目录的月份文件夹。preview.41 加入了这一目录布局。[独立 AFC 记录](../.timecut-build/iphone-full-20261124-clean/retrust-afc.json) · [Windows 文件流记录](../.timecut-build/iphone-full-20261124-clean/retrust-shell.json) · [WPD 目录筛选](../src-tauri/src/ingest/wpd.rs#L321)
- 第二部 iPhone 的 AFC 正常，收到了 19 项。WPD 完整清单有 22 项，14 项有匹配本地凭据，8 项尚未核销。两条通道有 13 项内容完全相同，总计保留了 20 个独立文件，逐份校验通过。[收件校验](../.timecut-build/iphone-full-preview41-20261125-080554/verified-local-payloads.json)
- Windows 清单有 4 个条目出现长度不一致。两个 MOV 返回的实际字节与 AFC 中各自同名的原片完全一致。另两个条目尚无相同原件凭据，其中一个 JPG 在 Time-cut 和资源管理器均读到声明长度的一半。其间 Windows 还返回了 ERROR_READ_FAULT。AFC 的补收成功记录不能填补所有未核销的 Windows 项，因而没有触发归档和可拔线提示。[异常回包对账](../.timecut-build/iphone-full-preview41-20261125-080554/returned-content-audit.json) · [资源管理器对照](../.timecut-build/iphone-full-preview41-20261125-080554/windows-stream-control.json) · [微软错误码定义](https://learn.microsoft.com/zh-cn/windows/win32/wpd_sdk/error-constants) · [Time-cut 完成条件](../src-tauri/src/ingest/capture.rs#L639)

Apple 说明 USB 导入时 HEIF 和 HEVC 可能转为 JPEG 和 H.264。第二部手机的两份 HEIC 在 Windows 清单中确有相同主文件名的 JPG。这支持两条接口能呈现不同版本的判断。由于第二部手机尚未完成「自动」与「保留原片」的同文件对照，转换、缓存或读取会话各自的责任仍待确定。[Apple USB 导入说明](https://support.apple.com/en-us/116944)

## 开源项目的做法

- **Rapid Photo Downloader + libimobiledevice/ifuse。** Linux 上的 Rapid Photo Downloader 自 0.9.27 起，为 iOS 从 gPhoto2/PTP 改用 Apple 文件服务以改善可靠性。v0.9.37 源码通过 idevicepair 配对、ifuse 挂载并按文件系统扫描。ifuse 从 AFC 取得文件大小，也从 AFC 读文件流。Rapid Photo Downloader 的下载历史按文件名、修改时间和大小匹配，适合快速预选。型号、格式或路径变动时，Time-cut 的内容摘要校验更能证明两份文件是否相同。[项目手册](https://damonlynch.net/rapid/documentation/) · [挂载实现](https://github.com/damonlynch/rapid-photo-downloader/blob/v0.9.37/raphodo/storage/storageidevice.py#L194-L218) · [AFC 文件属性](https://github.com/libimobiledevice/ifuse/blob/1.2.1/src/ifuse.c#L208-L233) · [AFC 读取](https://github.com/libimobiledevice/ifuse/blob/1.2.1/src/ifuse.c#L308-L338) · [历史数据库](https://github.com/damonlynch/rapid-photo-downloader/blob/v0.9.37/raphodo/rpdsql.py#L758-L775)
- **libgphoto2。** 上游复现过 iPhone 正常返回月份目录列表，但目录自己的父节点字段错误，导致文件夹全被过滤。维护者提交 9f5d4f9，根据根目录枚举接口返回的句柄修正 Apple 设备的父节点，报告者实测确认有效。这个补丁处理的是 PTP 枚举，说明应验证不同接口返回的信息，文件流短读仍需另外处理。[上游问题与实机确认](https://github.com/gphoto/libgphoto2/issues/1254#issuecomment-4762713925) · [上游实际修复](https://github.com/gphoto/libgphoto2/commit/9f5d4f9ca0a7f58bac7987180a48154ea07c090f)
- **FreeFileSync。** Windows 的成熟文件同步工具支持通过 MTP 同步 iPhone，提供内容比较和可选的复制后逐字节校验。其官方论坛已有 iPhone 文件流长度与声明不符、资源管理器同样读出零字节的实例。项目在 11.23 将 MTP 枚举中的子项错误延后处理。其手册说明复制后校验比较的是两端读出的字节，源数据本身的正确性仍需由来源证明。[MTP 支持](https://freefilesync.org/faq.php) · [复制后校验](https://freefilesync.org/manual.php?topic=expert-settings) · [短读案例](https://freefilesync.org/forum/viewtopic.php?t=7120) · [11.23 发布记录](https://freefilesync.org/archive.php)
- **Immich。** 在 iPhone 上选定相册后上传到服务器，按内容摘要识别已有资产。v3.4.2 的 iOS 摘要来自 PhotoKit 资源读取，常规上传取得文件的代码使用 originFile。上游仍有用户报告两条路径拿到不同字节，导致重复上传。这提示导入清单里的摘要最好对应实际写入电脑的那一份字节。[移动备份文档](https://docs.immich.app/features/mobile-backup/#deduplication) · [iOS 摘要源码](https://github.com/immich-app/immich/blob/v3.4.2/mobile/ios/Runner/Sync/MessagesImpl.swift#L402-L423) · [上传文件来源](https://github.com/immich-app/immich/blob/v3.4.2/mobile/lib/infrastructure/repositories/storage.repository.dart#L15-L30) · [上游报告](https://github.com/immich-app/immich/issues/29543)

## 对 Time-cut 的建议

1. **AFC 可稳定盘点时，优先建立 Apple 原件清单。** Time-cut 已实现 AFC 列目录和设备端 SHA-1，无需重造协议。原件收齐的承诺以该通道实际盘点的文件为准。Windows 独有的编辑版本、AAE 附件和不可匹配项目保留独立的待确认状态。AFC 无法枚举时，WPD 仍可接收它能验证的文件，并给出实际覆盖范围。[现有 AFC 校验实现](../src-tauri/src/ingest/afc.rs#L543) · [现有 Windows 优先路由](../src-tauri/src/ingest/mod.rs#L939)
2. **核对同一条目的实际内容。** 同路径或同名只用于找候选。对短读例外，同时核对对象标识、会话、来源原件摘要及本机摘要。考虑分别记录 WPD 对象属性长度、默认资源声明长度和实收长度。无法确认的项目单独重试，已校验文件继续保留。[微软 WPD 默认资源定义](https://learn.microsoft.com/en-us/windows/win32/wpd_sdk/wpd-resource-default)
3. **用两台 iPhone 覆盖不同故障。** 对第二部先用同一有问题的文件比较两种 USB 导出设置，再验证原件收齐、物理复插零新增以及只新增一项时的行为。第一部用于检验 AFC 不可读时 Windows 仍能保留已完成文件，并如实显示缺项。传输完成与拔线提示分别以实际数据和本轮读取状态为凭据。

# Time-cut

Time-cut 是一款 Windows 本地相册。它把手机和文件夹里的照片、视频保存到电脑，按内容去重，并提供时间线、人物、地点和主题浏览。项目基于 [Lap v0.4.1](https://github.com/julyx10/lap/tree/v0.4.1)。

## 下载和测试

在 [Time-cut Releases](https://github.com/code-lawyer/time-cut/releases) 下载 Windows x64 便携 ZIP，解压后运行 `Time-cut.exe`。更新前从托盘退出，解压新版到新目录。每个 Release 提供 ZIP 的 SHA-256。当前为预览版，EXE 未签名。

从零测试时，在应用内选择一个新的空文件夹作为归档目录。安卓解锁后将 USB 设为「文件传输」，iPhone 解锁并信任电脑。启用自动导入后，等界面提示「相册原件已收齐，可以拔线」，再等待电脑完成整理。再次连接同一台手机，可检查不变的文件是否复扫零新增。

- 安卓通过 USB 读取各存储卷的 `DCIM`、`Pictures` 和 `Movies`。iPhone 读取 `DCIM` 相机目录。
- 在电脑上也可以导入本地文件夹，或选择「原位索引」直接浏览现有文件。原件继续保存在用户选定的位置。
- 本机模型对照片进行主题分类和人物分组。主题可以手动改，人物可以先匿名分组再命名。

[Windows 使用与构建说明](docs/Time-cut-Windows.md) · [隐私说明](PRIVACY.md)

## 源码和许可

用 `git clone --recurse-submodules https://github.com/code-lawyer/time-cut.git` 检出源码。构建便携版的步骤、依赖和验证命令见 [Windows 指南](docs/Time-cut-Windows.md)。Windows 工作流负责从匹配版本的源码标签生成并测试发行包。

Time-cut 继承 [GPL-3.0-or-later 许可](LICENSE)。[第三方声明](THIRD_PARTY_NOTICES.txt)列出随包组件、来源和适用许可。

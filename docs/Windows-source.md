# Windows 便携版对应源码

Time-cut 使用 GPL-3.0-or-later。每个 Windows Release 提供该版本便携 ZIP、源码包和各自的 SHA-256。便携包中的 `BUILD-INFO.json` 标明应用提交、子模块提交和 EXE 摘要。源码包 `Time-cut-<版本>-source.tar.gz` 包含同一提交的源码、锁文件、构建脚本和四个已展开的原生子模块。根目录的 `TIME-CUT-SOURCE.json` 可用于对照这些修订。[Time-cut Releases](https://github.com/code-lawyer/time-cut/releases)

## 重建应用和本地静态库

解开源码包，在 Visual Studio x64 Developer PowerShell 中按照随包使用说明的「从本地源码构建」安装固定依赖并构建。源码包中可跳过 `git submodule update`。如使用 Git，检出与下载版本一致的 `timecut-v<版本>` 标签，并执行 `git submodule update --init --recursive`。

Windows 程序静态链接 LibRaw、libjpeg-turbo、libheif 和 libde265。完整库源码位于 `src-tauri/third_party/`，原生编译、链接及功能选项在 `src-tauri/build.rs`。要修改或替换这些库，请在新解开的源码目录修改相应库并按同一命令重建应用。可在本机重新运行自己的 EXE。包内许可证目录包含各库声明和许可证。Windows Rust 和前端依赖的实际版本、作者、许可证与文本见 `Windows-DEPENDENCIES.txt`。依赖解析分别由 `src-tauri/Cargo.lock`、`src-vite/pnpm-lock.yaml` 固定。

## FFmpeg 和 ffprobe

从 0.1.0-preview.39 起，Windows x64 使用 FFmpeg-Builds 的 `win64 gpl 8.1` 构建。FFmpeg 完整提交为 `a8029b6528fecbcb5d99b331cfc76e6467b9bc77`，构建配方提交为 `b9ebe81995b58cd4e3177a6c0ad5dc2392f7f7fd`。两份程序、上游 ZIP、构建记录和输入摘要由源码中的 `scripts/windows_ffmpeg.json` 标识。[上游构建记录](https://github.com/FFmpeg-Builds/FFmpeg-Builds/actions/runs/45390806074)

Release 附有 `FFmpeg-8.1.3-a8029b65-win64-source-part1.zip`、`part2.zip`、`part3.zip`（后两份使用相同文件名前缀）及 `FFmpeg-8.1.3-a8029b65-win64-source.sha256`。按校验文件核对三份 ZIP 后，将它们解压到同一空目录。三份合起来包含 FFmpeg 源码、原始配方、补丁和上游本次构建实际启用的 102 份依赖源码。在安装了 Docker Buildx、Bash、Python 3 和 GNU tar 的 Linux x86_64 主机运行 `bash rebuild_ffmpeg_from_source.sh` 可使用随附源码重建。Docker 需要获取基础系统/工具链镜像。程序的实际编译选项可用 `ffmpeg -buildconf` 查看。

如果上游 ZIP 已撤下，下载脚本会查找最近公开的 Time-cut Windows x64 包，包括预览版。也可以运行 `./scripts/download_ffmpeg_sidecar.ps1 -Arch x64 -Archive <上游或Time-cut便携ZIP>` 从本地 ZIP 恢复固定版本。脚本逐个核对可执行文件的 SHA-256。要使用自行修改的 FFmpeg，可替换自己解压目录下 `ffmpeg` 内相应文件。项目依赖声明保留发布版的固定摘要，供用户验证官方下载。

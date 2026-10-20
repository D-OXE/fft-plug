# fft-plug

项目总体说明见：[PROJECT_SUMMARY.md](PROJECT_SUMMARY.md)。

`fft-plug` 是一个面向 **niri + Wayland** 桌面的实时音频可视化组件。它从 PipeWire 读取当前系统音频输出，将 PCM 数据转换为频谱快照，并以柱状图或连续曲线显示在桌面底部。

> 当前状态（2026-10-20）：MVP 主链路已在真实 niri 26.04 + PipeWire 1.6.9 会话完成验收——实际音频峰值、静音睡眠与唤醒、默认输出切换、PipeWire 重启、输出设备移除恢复、layer-shell 契约、GL/surface 重建、整数与分数缩放、60/120/160 Hz 刷新率以及 10 分钟 active + 5 分钟静音的长时间运行均已实测。逐柱毛玻璃（V3）在当前 GTK 4.24 下不可实现，程序以透明彩虹柱降级；详细证据与限制见 [docs/validation/MVP_ACCEPTANCE.md](docs/validation/MVP_ACCEPTANCE.md)。

## 项目定位

`fft-plug` 不修改 niri、不注入 compositor，也不依赖私有插件 ABI。它是一个独立的 Wayland 客户端：

```text
PipeWire 默认输出 monitor
        │
        ▼
  实时 PCM 采集
        │  预分配 SPSC ring buffer
        ▼
  混音 / 加窗 / real FFT
        │
        ▼
  频段聚合 / dB / 平滑
        │  latest snapshot
        ▼
 GTK4 + gtk4-layer-shell
        │
        ▼
 GtkGLArea + OpenGL/GLSL
        │
        ▼
  Bars / Curve renderer
```

这里的“plug”表示桌面组件，不表示 niri 的进程内插件。

## 当前实现

当前程序已经包含从 PipeWire 采集到 OpenGL 绘制的端到端路径。命令行提供显示模式和动态 profile 选择：

```bash
cargo run -- --mode bars
cargo run -- --mode curve
cargo run -- --debug --mode bars
cargo run -- --mode bars --dynamics soft
cargo run -- --mode bars --dynamics balanced
cargo run -- --mode bars --dynamics punchy
```

`--dynamics` 默认为 `punchy`，可选 `soft`、`balanced`、`punchy`。发布构建后可直接运行 `target/release/fft-plug --mode bars --dynamics punchy`。

音频流通过 PipeWire 的默认输出 monitor 自动连接。程序优先请求交错 `F32`，但仍从实际协商结果读取采样率、声道数和格式；当前采集回调支持常见交错与平面整数、浮点格式，并将多声道混合为单声道。音频只在内存中经过固定容量、有序的 PCM FIFO；FIFO 过载时丢弃最旧的 READY block 并标记 discontinuity，DSP 会在下一个 block 前重置分析窗口。DSP 只向 UI 发布最新频谱快照，并在无 PCM 时阻塞等待、由新 block 事件唤醒。

`--debug` 每秒输出一次低频诊断快照，包含 PipeWire session/reconnect 计数、callback、PCM 写入/消费/队列/overrun、DSP frame、snapshot age、activity、渲染帧、frame age 以及 surface 逻辑尺寸。当前尚未提供设备选择参数、多显示器选择参数或配置文件；这些属于后续工作。没有 Wayland layer-shell 会话时，程序会给出可重试的启动错误。

## MVP

首个可用版本包含：

- 自动捕获 PipeWire 当前默认输出的 monitor；
- 从实际协商结果读取采样率、声道数和格式，不硬编码 48 kHz / stereo；
- 多声道混合为单声道，使用 Hann 窗和实数 FFT 生成频谱；
- 将 FFT bin 聚合为视觉频段，并使用 dB 标尺和可选的 `soft` / `balanced` / `punchy` attack/release 平滑；默认是 `punchy`（15 ms / 120 ms）；
- 提供 `bars` 与 `curve` 两种显示模式；
- renderer 不再重复执行 45/220 ms 音乐动态，只用约 8 ms 的对称插值连接 DSP snapshot；
- 使用 `wlr-layer-shell`，默认位于 `bottom` 层，不抢键盘焦点、不申请独占区域；
- 窗口可被普通应用覆盖，因此不会永久浪费桌面空间；
- 静音后平滑衰减并进入低活动状态，完成淡出后移除 GTK frame tick，恢复音频时自动唤醒并重新安装；
- 默认输出切换、PipeWire 短暂断开、暂时没有音频设备时不 panic，并尽量自动恢复；
- 缓冲区和内存使用始终有明确上限。

MVP 暂不包含：

- 歌词、播放器控制、均衡器或音频效果处理；
- 麦克风录音、文件分析、网络音频输入；
- 频谱数据持久化；
- 完整设置 GUI、主题市场、用户自定义 shader；
- X11/XWayland 专用后端；
- niri 源码修改或 compositor 注入；
- Vulkan / wgpu 后端。

## 技术栈

### 语言

- Rust stable

### 音频

- PipeWire 原生 API + Rust bindings
- 首选系统默认输出 monitor
- process callback 只读取、轻量转换和发布采样

### DSP

- `realfft` / `rustfft`
- Hann window
- 单边幅值归一化
- dB 映射与有限值保护
- 对数或近似对数频段
- `soft` / `balanced` / `punchy` attack / release 时间常数

### 实时数据传递

- PipeWire → DSP：预分配、容量固定、保持 block 顺序的 SPSC PCM FIFO；过载时丢弃旧 block 并显式标记 discontinuity
- DSP → Renderer：只发布最新不可变频谱快照，不排队历史帧

### 桌面集成

- GTK 4：应用生命周期、Wayland/HiDPI、主循环
- `gtk4-layer-shell`：`wlr-layer-shell`
- namespace：`fft-plug`

### 渲染

- 当前实时 renderer 使用 `GtkGLArea + OpenGL + GLSL`
- GTK 负责 surface，OpenGL 负责频谱视觉

`DrawingArea/Cairo` 不是当前运行路径；如果需要单独调试 layer-shell，仍可作为后续 bring-up 工具。

OpenGL 是当前目标渲染后端。这个项目的图形复杂度远不足以证明 Vulkan 的额外工程成本；除非以后出现明确的第二个 renderer 或测量证据，否则不增加 wgpu/Vulkan 抽象层。

## 数据与线程模型

```text
PipeWire thread              DSP worker               GTK main thread
┌──────────────┐         ┌───────────────┐         ┌─────────────────┐
│ read PCM     │ ──────▶ │ mix/window/FFT│ ──────▶ │ frame + OpenGL  │
        └──────────────┘  FIFO   └───────────────┘ latest  └─────────────────┘
```

核心规则：

- PipeWire callback 不 FFT、不绘图、不日志、不阻塞；
- realtime path 不依赖无界分配；
- DSP 落后时只在有界 FIFO 满时丢弃最旧 block，并让 DSP 重置跨断点状态，不能积累无限延迟；
- renderer 永远读取“最近一次完整快照”；
- GTK 对象只能在 GTK 主线程访问；
- 分析更新率和显示刷新率相互独立。

## 频谱基线

当前实现采用以下工程基线；真实视觉测试和性能测量完成后再决定是否调整：

| 参数 | 当前值 | 说明 |
| --- | --- | --- |
| FFT window | 4096 samples | 优先保证低频可视分辨率 |
| hop size | 512 samples | 48 kHz 下约 93.75 次分析/秒 |
| window | Hann | 降低频谱泄漏 |
| bands | 64 | 根据可用 FFT bin 自动约束 |
| min frequency | ~20 Hz | 实际受 FFT 分辨率限制 |
| max frequency | `min(20 kHz, Nyquist)` | 根据运行时采样率计算 |
| amplitude | dB | 设可调整的显示下限 |
| smoothing | attack / release | 使用时间常数而非固定每帧系数 |

### 低频分辨率

频段不能简单把 20 Hz–20 kHz 等比切成固定数量后强行映射。实现必须根据：

```text
bin_width = sample_rate / fft_size
```

预计算有效 bin 范围，避免多个显示频段反复落到同一 FFT bin，或产生大量空频段。

如果 4096 点的时域响应在真实使用中显得过软，再实测 2048 点；不要只凭直觉切换窗长。

## 绘制节奏

DSP 频率不等于 GPU 刷新率。

- FFT 更新由 `hop size` 决定；
- GTK/OpenGL 绘制应跟随 GTK frame clock / 显示器刷新节奏；
- renderer 在两个 DSP snapshot 之间只做约 8 ms 的对称视觉插值，不重复施加长时 attack/release；
- 不使用固定 60/120 FPS timer 作为架构前提；
- 活动或淡出期间保留 GTK frame clock，睡眠且淡出完成后移除 tick callback；
- 可设置最大绘制上限用于节能，但必须与实际显示刷新率解耦。

## Silence Mode

桌面组件不应该在没有声音时持续占用 GPU。

```text
ACTIVE
  │ audio falls below threshold
  ▼
DECAYING
  │ spectrum -> 0, alpha -> 0
  ▼
SLEEPING
  │ audio returns
  └──────────────▶ ACTIVE
```

预期行为：

- `ACTIVE`：正常 FFT 与绘制；
- `DECAYING`：完成平滑回落与淡出；
- `SLEEPING`：停止持续 GL redraw，并移除 frame tick，仅保留恢复音频所需的轻量检测；
- 新音频出现后立即恢复分析与绘制。

具体阈值、保持时间和淡出时间在真实设备上调整。

## 显示模式

### Bars

经典频谱柱，作为首个端到端 renderer：

```text
        █
    █   █
 ▃  █ █ █ ▄
████████████████
```

### Curve

与 Bars 共享完全相同的频谱快照，只改变几何构造：

```text
        ╭──╮
     ╭──╯  ╰─╮
 ╭───╯        ╰──╮
─╯                 ╰──
```

曲线插值必须限制过冲，不允许产生负高度、NaN 或超出归一化范围的异常坐标。

后续可考虑 waveform、mirror、radial、glow、bloom、particles，但这些不是 MVP 的前置条件。

## niri 集成

基本启动方式：

```bash
fft-plug
fft-plug --mode bars
fft-plug --mode curve
```

niri 会话启动：

```kdl
spawn-at-startup "fft-plug"
```

程序只使用 Wayland layer-shell，不回退到 X11。如果桌面环境显式设置了 `GDK_BACKEND=x11`，请使用 `GDK_BACKEND=wayland fft-plug` 启动。

程序使用固定 namespace：

```text
fft-plug
```

项目提供可直接复制或通过 niri `include` 引入的配置片段：[niri/fft-plug.kdl](niri/fft-plug.kdl)。它不会修改用户的 `~/.config/niri`，也不包含其他窗口规则或 `opacity` 设置：

```kdl
layer-rule {
    match namespace="^fft-plug$"

    background-effect {
        xray true
    }
}
```

可以复制上面的内容，或在 niri 配置中按实际路径 include `niri/fft-plug.kdl`。这里刻意不写 `blur true`：那是 niri 按整层几何启用的矩形模糊，会覆盖整个 `fft-plug` layer，把柱间空隙也一起模糊。

**当前限制（2026-10-20 实测，见验收文档）**：GTK 4.24 会在创建窗口 surface 时自行创建 `ext_background_effect_surface_v1`，而 `ext-background-effect-v1` 规定每个 `wl_surface` 只能有一个该对象；GTK 也没有公开设置 blur region 的 API，且自身从不提交 region。因此 fft-plug 无法提交逐柱 blur region：旧实现会对同一 surface 重复创建对象，niri 直接以 `wl_display.error`（“wl_surface already has a background effect object attached”）断开连接，所以本轮移除了该实现。上面这段 layer-rule 在当前环境下不会产生可见模糊，程序以**透明彩虹柱**作为正式降级路径运行。等 GTK 提供 region 控制后，该规则即可重新生效。

MVP 默认：

```text
layer            bottom
exclusive zone   0
keyboard focus   none
anchor           left + right + bottom
height           320 logical px
width            由 compositor 根据 output 协商
```

普通窗口可以遮挡它；这属于产品设计，不是缺陷。

## 实际目录

```text
src/
├── main.rs          # CLI、生命周期、组件装配
├── audio/           # PipeWire 枚举、格式协商、采样
├── dsp/             # 混音、窗函数、FFT、频段、平滑
├── pipeline/        # SPSC PCM FIFO、latest snapshot、状态协调
├── shell/           # GTK application、layer-shell、GLArea 生命周期
└── render/          # OpenGL 资源、几何、shader、Bars/Curve
```

## 开发环境

至少需要：

- Rust stable toolchain；
- PipeWire 开发文件；
- GTK 4 开发文件；
- gtk4-layer-shell 开发文件；
- OpenGL/EGL 运行与开发环境；
- `pkg-config`。

Arch Linux 可使用：

```bash
sudo pacman -S --needed rust pkgconf pipewire gtk4 gtk4-layer-shell mesa
```

常用检查：

```bash
cargo fmt --check
cargo check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

纯 DSP 单元测试不得要求真实 PipeWire 服务或 Wayland 会话。

### Arch Linux 打包

仓库根目录提供 `PKGBUILD` 与 `make-source-tarball.sh`。二者都不修改系统，也不联网；生成的压缩包与源码 tarball 均被 `.gitignore` 忽略：

```bash
./make-source-tarball.sh          # 从当前工作区生成 fft-plug-<version>.tar.gz
makepkg -f                        # 生成 fft-plug-<version>-<rel>-x86_64.pkg.tar.zst
sudo pacman -U fft-plug-0.1.0-1-x86_64.pkg.tar.zst
```

包内包含 `/usr/bin/fft-plug`、许可证、README、验收记录以及 `niri-fft-plug.kdl` 配置样例。
`PKGBUILD` 使用 `options=(!lto)`：Cargo 自行处理 Rust 侧 LTO，而 makepkg 全局 `lto` 选项会把
`-flto=auto` 追加到 `CFLAGS`/`LDFLAGS`，使 `cc` 编译的 libspa 静态 shim 变成 LTO 字节码，
最终 rustc 链接（不传递 `LDFLAGS`）无法解析其符号。

## 已实现与待验证

已由代码和无设备测试覆盖：

- `fft-plug` layer-shell namespace、bottom 层、无键盘焦点、exclusive zone 0；
- PipeWire 默认输出 monitor 的运行时格式读取和自动恢复循环；
- 4096 点 FFT、512 点 hop、Hann 窗、64 个有效频段、dB floor 和时间驱动的 dynamics profile 平滑；
- `soft` / `balanced` / `punchy` profile、8 ms renderer 视觉插值，以及活动期间 frame clock / 睡眠期间无 tick 的切换；
- Bars / Curve 共用同一份 `SpectrumSnapshot`；
- 有序有界 PCM FIFO、overrun/discontinuity 计数与 analyzer reset、事件唤醒 DSP worker，以及 latest-only snapshot；
- 左右 anchor 的全宽 layer-shell surface，默认高度为 320 logical px；
- `--debug` 低频诊断输出，包含 `audio.sessions`、`audio.reconnects`、callback、PCM、DSP frame、`dsp.snapshot_age_ms`、activity、render frame、`render.frame_age_ms` 和实际 surface 尺寸；
- 静音 `ACTIVE -> DECAYING -> SLEEPING -> ACTIVE` 状态；
- Bars 的程序化连续彩虹流光（30 s 色相周期、HSL finite/clamp）、半透明材质与顶部高光；Curve 保持原有固定配色和频谱语义；
- 静音、单频、多频、多声道、极低幅值、不同采样率、窗长归一化、边界输入、attack/release profile 和 worker 唤醒测试；
- 彩虹 phase 的刷新率分割等价、回绕、非有限输入和 RGB 范围测试；
- Bars 顶点数量/有限性、材质与生命周期 alpha 分离，以及 ACTIVE/DECAYING/SLEEPING phase 行为测试；
- `cargo test --all-targets --all-features` 的纯逻辑测试覆盖 DSP、pipeline（含 snapshot age）、renderer（含 frame age）和彩虹颜色逻辑，共 32 个测试。

本轮已完成的真实桌面验收：

- 440 Hz 测试音 `peak=0.350`、DSP band 峰值 404.1 Hz（band index 3）、snapshot age 2–5 ms；
- 静音后 `DECAYING -> SLEEPING`，`render.frames` 冻结、CPU 0.1%、RSS 稳定；恢复音频后 1 秒内回到 `ACTIVE`；
- 默认输出 HECATE ↔ HDMI 切换时采集 stream 透明跟随（`sessions` 仍为 1），PipeWire 服务重启后 `sessions=2 reconnects=1` 并自动重连；全部输出设备移除时保持 `Connected` 且不崩溃；
- `niri msg layers` 确认 bottom layer、namespace `fft-plug`、无键盘交互；输出 off/on 触发 surface 与 GL 资源重建；
- 160/120/60 Hz 下 render 帧率分别为 160/120/60；scale 2 与 1.5 下柱体满宽、无接缝；
- 10 分钟 active 负载 CPU 17–19%、RSS 127.9→128.0 MB、0 overrun/drop；5 分钟静音 CPU 0.1%、RSS +56 kB。

仍需验证或评估：

- 逐柱毛玻璃（V3）：受 GTK 4.24 限制阻塞，当前以透明彩虹柱降级；
- 多显示器默认行为（当前环境只有单输出）；
- 壁纸轮换与明暗/高对比/高饱和壁纸对照（未修改用户桌面）；
- 长时间测试按用户批准缩短为 10 + 5 分钟（计划原定 30 + 10）。

## 后续方向

MVP 稳定后再评估：

- Oklch/Oklab 感知均衡色环（仅在真实 A/B 证明 HSL 亮度不一致后评估）、壁纸/专辑取色；
- glow / bloom；
- waveform / mirror / radial；
- 多显示器独立 surface；
- 指定 PipeWire sink；
- 配置文件；
- systemd user service 与发行版打包；
- MPRIS 封面与歌曲状态；
- 其他 renderer。

## 隐私与安全

`fft-plug` 只在内存中分析实时系统输出。默认不保存音频、不保存频谱、不联网。程序只请求音频采集与 layer-shell 显示所需的本地权限。

## 许可证

项目使用 MIT 许可证，完整文本见 `LICENSE`。

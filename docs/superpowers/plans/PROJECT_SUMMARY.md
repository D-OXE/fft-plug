# fft-plug 项目总结

> 本文基于当前源码、`README.md`、`PLAN.md`、`Cargo.toml` 和 niri 配置片段整理。
> 它描述当前实现与工程边界，不把尚未实现的后续方向当作现有功能。

## 1. 项目概述

`fft-plug` 是一个面向 **niri + Wayland** 的独立桌面音频频谱可视化组件。

它从 PipeWire 读取当前系统默认输出的 monitor 音频，在内存中完成：

```text
PipeWire 音频流
    ↓
PCM 有界传输
    ↓
混音、加窗、FFT
    ↓
频段聚合、dB 映射、动态平滑
    ↓
最新频谱快照
    ↓
GTK layer-shell + GtkGLArea
    ↓
OpenGL / GLSL
    ↓
Bars 或 Curve
```

项目名称中的 `plug` 表示桌面组件，而不是 niri 的进程内插件。程序不修改 niri 源码、不注入 compositor，也不依赖私有插件 ABI。

## 2. 产品边界

### 已确定的范围

- 只面向 Wayland，不实现 X11/XWayland 专用后端；
- 使用 `wlr-layer-shell`，默认显示在 `bottom` layer；
- 音频源是 PipeWire 当前系统默认输出的 monitor；
- PCM、频谱和壁纸像素不写入磁盘，不联网；
- 提供 Bars 和 Curve 两种显示模式；
- 支持静音衰减、睡眠和自动唤醒；
- 毛玻璃由 compositor 提供，程序不读取窗口后方像素；
- 当 compositor 不支持背景效果协议时，仍应显示透明频谱柱，而不是启动失败。

### 明确不属于 MVP

- 歌词、播放器控制、MPRIS、均衡器和音频效果器；
- 麦克风录音、文件分析、网络音频输入；
- 设置中心、主题市场、用户 shader；
- niri IPC、niri 源码修改或 compositor 注入；
- Vulkan/wgpu 第二渲染后端；
- 多 pass bloom、粒子系统和复杂后处理。

## 3. 技术栈

| 层次 | 技术 | 作用 |
| --- | --- | --- |
| 语言 | Rust stable，Edition 2024 | 内存安全、线程安全和系统级性能 |
| 音频 | PipeWire Rust bindings | 连接默认输出 monitor，读取协商后的实时 PCM |
| DSP | `realfft`、`rustfft` | 实数 FFT、幅值计算和频谱分析 |
| 桌面应用 | GTK 4 | 应用生命周期、主循环、HiDPI 和 `GtkGLArea` |
| Wayland shell | `gtk4-layer-shell` | 使用 `wlr-layer-shell` 设置 layer、anchor、namespace 等属性 |
| 图形 | OpenGL、GLSL、`gl` crate | 实时几何绘制和颜色渐变 |
| 背景效果 | 无（已移除 `wayland-client` / `wayland-protocols` 依赖） | 原计划用 `ext-background-effect-v1` 提交逐柱 blur region；GTK 4.24 自行占用该 surface 对象，客户端无法再提交，详见第 8 节 |
| CLI | `clap` | 解析显示模式、动态 profile 和 debug 参数 |

默认启用 `desktop` feature。`src/lib.rs` 保留 DSP 与 pipeline，使纯逻辑测试不必依赖 GTK、Wayland、PipeWire 或真实显示会话。

## 4. 总体设计思路

项目的核心设计是把实时音频、分析计算、桌面生命周期和 GPU 显示解耦。

```text
PipeWire callback        DSP worker             GTK main thread
┌───────────────┐       ┌───────────────┐      ┌──────────────────┐
│ 读取协商格式  │       │ FIFO 消费      │      │ frame clock      │
│ 轻量解码混音  │──────▶│ 窗口化 / FFT   │─────▶│ latest snapshot  │
│ 写入固定 FIFO │       │ 频段 / 平滑    │      │ OpenGL 绘制       │
└───────────────┘       └───────────────┘      └──────────────────┘
```

设计上遵循几条原则：

1. **实时路径只做实时路径需要的工作。** PipeWire callback 不做 FFT、绘图、文件 I/O、日志输出或阻塞等待。
2. **延迟有上限。** 音频传输使用固定容量 FIFO，处理跟不上时丢弃旧数据并显式标记断点，不用无界缓存掩盖延迟。
3. **新鲜度优先于历史完整性。** DSP 到 UI 只保存最新完整频谱快照，UI 不追赶已经过时的频谱帧。
4. **分析节奏与显示节奏分离。** FFT 按 hop size 更新，renderer 跟随 GTK frame clock；renderer 不重复执行音乐动态平滑。
5. **平台生命周期明确。** GTK 管理窗口和 surface，OpenGL 资源只在有效 context 中创建和销毁；surface 重建（输出关闭/开启）时 GL 资源随之重建。
6. **降级路径正式存在。** layer-shell、OpenGL 或 PipeWire 不可用时给出可操作错误；背景 blur capability 缺失不应破坏频谱柱本身。

## 5. 模块结构与职责

```text
src/
├── main.rs
├── lib.rs
├── audio/mod.rs
├── dsp/mod.rs
├── pipeline/mod.rs
├── shell/mod.rs
└── render/
    ├── mod.rs
    ├── color.rs
    └── background_effect.rs
```

### `src/main.rs`

负责 CLI、模块装配和关闭顺序：

1. 解析 `--mode`、`--dynamics` 和 `--debug`；
2. 创建 DSP pipeline；
3. 启动 PipeWire capture；
4. 启动 GTK layer-shell；
5. 收到退出请求后停止音频、关闭 pipeline 并等待线程结束。

### `src/audio/mod.rs`

负责 PipeWire 侧的连接与采样读取：

- 创建 PipeWire main loop、context、core 和 capture stream；
- 使用默认输出 monitor 自动连接；
- 首选协商交错 `F32`，但最终以 PipeWire 实际协商结果为准；
- 支持常见交错/平面整数、浮点格式的解码；
- 将多声道混合为单声道；
- process callback 只进行 buffer 读取、轻量解码和写入 `PcmRing`；
- 监听格式变化、stream 状态变化和断开事件；
- PipeWire 会话出错时清理并进入重连循环；
- 通过 `AudioStatus` 以原子计数记录 callback、丢弃、格式、峰值等诊断信息。

### `src/dsp/mod.rs`

负责纯 DSP 分析，不依赖 GTK、Wayland 或 PipeWire：

- 混音后的单声道 PCM 进入固定窗口；
- 使用 Hann window；
- 使用 real FFT 生成单边频谱；
- 按窗增益和 DC/Nyquist 规则进行幅值归一化；
- 将幅值转换为带 floor 的 dB；
- 根据运行时采样率、FFT size 和 Nyquist 动态生成合法 band mapping；
- 默认生成 64 个视觉频段；
- 每个频段使用归一化 level 和 attack/release 时间常数平滑；
- 对 NaN、Inf、零幅值和极低幅值进行 finite/floor 保护；
- 输出 `SpectrumSnapshot`，包含采样率、递增 frame index 和频段列表。

当前默认 DSP 参数：

| 参数 | 值 |
| --- | ---: |
| FFT window | 4096 samples |
| hop size | 512 samples |
| band count | 64 |
| 最低频率 | 约 20 Hz，受 bin 分辨率约束 |
| 最高频率 | `min(20 kHz, Nyquist)` |
| dB floor | -80 dB |
| 默认 dynamics | Punchy，attack 15 ms / release 120 ms |

在 48 kHz 下，4096 点窗口约覆盖 85.3 ms，512 点 hop 约每 10.67 ms 产生一次分析结果。FFT 更新率与显示器刷新率没有绑定关系。

### `src/pipeline/mod.rs`

负责跨线程传输、状态协调和 shutdown：

- `PcmRing` 是 8 个固定槽位组成的 SPSC FIFO；
- 单槽最大保存 8192 个 PCM frame；
- 槽位状态为 `FREE → WRITING → READY → READING → FREE`；
- FIFO 满时优先回收最旧的 READY block；
- 如果消费者正在读取，生产者记录 rejected overrun；
- block 被截断、覆盖或拒绝时设置 discontinuity；
- DSP 在下一个 block 前重置 analyzer，避免把断点两侧样本拼进同一个 FFT window；
- worker 通过 `park/unpark` 等待新 PCM，不使用高频空转轮询；
- `SnapshotStore` 只保留最近一个 `Arc<SpectrumSnapshot>`；
- `ActivitySignal` 管理 `ACTIVE / DECAYING / SLEEPING` 与唤醒请求。

静音状态的默认判定：

```text
quiet threshold       0.0008
wake threshold        0.0012
decay begins          0.12 s
sleep after           0.55 s
```

使用不同的睡眠唤醒阈值可以降低临界音量附近的状态抖动。

### `src/shell/mod.rs`

负责 GTK application 和 layer-shell：

- 检查当前 GDK 后端必须是 Wayland；
- 检查 `wlr-layer-shell` 是否可用；
- namespace 固定为 `fft-plug`；
- layer 固定为 `bottom`；
- anchor 为 `left + right + bottom`；
- exclusive zone 为 0；
- keyboard interactivity 为 none；
- 默认高度为 320 logical px，宽度交给左右 anchor 与 compositor 协商；
- `GtkGLArea` 使用手动 render 和 realize/unrealize 回调；
- 活动或淡出期间使用 GTK tick callback；
- 睡眠且淡出完成后移除 tick callback；
- 通过约 80 ms 的轻量 wake poll 检查 DSP 唤醒请求，并重新安装 frame callback。

### `src/render/mod.rs`

负责 OpenGL resource、shader、几何和动画状态：

- 在 GLArea realize 时创建 shader program、VAO 和 VBO；
- 在 unrealize 时删除 context 相关资源；
- 只消费 `Pipeline::latest_snapshot()`；
- 在相邻 DSP snapshot 之间做约 8 ms 的视觉插值；
- 不在 renderer 中重复执行长时间 attack/release；
- 所有顶点坐标、颜色和 alpha 都进行有限值与范围约束；
- Bars 与 Curve 使用相同的频谱数据语义；
- shader 编译/链接失败会返回启动错误。

Bars 的视觉实现包括：

- 64 根频谱柱的固定水平分布和间隙；
- 由横向位置与 phase 生成的连续彩虹色；
- 30 s 的循环色相动画；
- 半透明柱体材质；
- 柱顶高光；
- 生命周期 alpha 与材质 alpha 分离；
- 不再绘制整层透明遮罩。

Curve 只改变几何构造，不改变频谱快照、峰值含义和 DSP 语义。

### `src/render/color.rs`

包含不依赖桌面和时钟的纯颜色函数：

- phase 回绕与推进；
- 位置 + phase 到 hue 的映射；
- 受限 HSL 到 RGB；
- finite 与 `[0, 1]` clamp。

这样颜色逻辑可以独立测试，也不会把壁纸、主题或环境变量引入 renderer。

### `src/render/background_effect.rs`（已移除）

该模块曾负责逐柱毛玻璃：从 GTK surface 取原生 `wl_surface`、绑定
`ext_background_effect_manager_v1`、把每根柱子的 level 转成 surface-local `wl_region`。

2026-10-20 实测发现：GTK 4.24 在创建窗口 surface 时已经自行调用
`get_background_effect(wl_surface)`（一个 5 行纯 PyGObject 窗口程序同样触发），而
`ext-background-effect-v1` 规定每个 `wl_surface` 最多一个 `ext_background_effect_surface_v1`。
模块再次创建该对象会让 niri 发送 `wl_display.error`（"wl_surface already has a background effect
object attached"），GTK 报 `Error 71 (协议错误)` 并退出——程序在 niri 上无法启动。GTK 也未公开
设置 blur region 的 API。因此模块与其依赖被移除，透明彩虹柱成为唯一路径；`niri/fft-plug.kdl`
仍保留，但在当前 GTK 下不产生可见模糊。

niri 配置片段只保留：

```kdl
layer-rule {
    match namespace="^fft-plug$"

    background-effect {
        xray true
    }
}
```

这里不配置 `blur true`。niri 自己的 `blur true` 只知道整个 layer surface 的矩形几何，会把柱间空隙也模糊；逐柱形状由 `ext-background-effect-v1` 提交。

## 6. 生命周期与错误恢复

### 启动顺序

```text
CLI
  → Pipeline / DSP worker
  → PipeWire capture thread
  → GTK application
  → layer-shell surface
  → GtkGLArea realize
  → OpenGL resources
  → frame-clock rendering
```

### 正常关闭

```text
停止接收新采样
  → 唤醒并结束 DSP worker
  → 停止 GTK render/tick 回调
  → 删除 GL resources
  → 销毁 GTK / PipeWire 对象
```

### 主要恢复路径

- PipeWire stream 断开：更新状态、退出当前 session、等待后重连；
- 协商格式变化：更新采样率、声道和格式代数，DSP worker 按新采样率重建 analyzer；
- FIFO overflow：丢弃旧 block、记录 overrun、重置 FFT 时域状态；
- 暂时无音频：进入 decay/sleep，但保留恢复所需的轻量路径；
- 新音频到达：请求 GTK 唤醒并恢复 Bars/Curve 绘制；
- 背景效果：当前客户端不提交 blur region（GTK 4.24 占用 surface 对象），始终使用透明彩虹柱；
- GL shader 或 context 创建失败：返回带有具体原因的启动错误，不继续使用无效资源。

## 7. CLI 与用户可见行为

```bash
fft-plug [OPTIONS]

--mode <bars|curve>
--dynamics <soft|balanced|punchy>
--debug
```

默认行为：

- `--mode bars`；
- `--dynamics punchy`；
- 自动连接默认输出 monitor；
- 显示在屏幕底部，不占用 exclusive zone；
- 普通窗口可以覆盖频谱；
- 没有声音时淡出并停止持续 GL redraw；
- 有新的音频时自动恢复。

## 8. 验证策略与当前状态

### 纯逻辑测试覆盖

- 静音、极低幅值与 dB floor；
- 440 Hz 单频、多频叠加和峰值频段；
- 44.1 / 48 / 96 kHz；
- 2048/4096 等窗长相关归一化；
- band mapping 非空、合法、单调且不重复空 bin；
- 多声道混音；
- attack/release 上升和回落；
- analyzer format rebuild 和 discontinuity reset；
- FIFO 顺序、覆盖、拒绝和读取期间的所有权；
- Silence Mode 状态转换与 worker 唤醒；
- 彩虹 phase 的回绕、刷新率分割等价和 finite RGB；
- Bars 顶点有限性、alpha 分离和不绘制整层遮罩；
- 预乘 alpha 与 `ONE/ONE_MINUS_SRC_ALPHA` 混合（见第 8 节视觉缺陷修复）；
- `--debug` 的 session/reconnect、snapshot age 和 frame age 统计。

### 工程检查

常用检查命令：

```bash
cargo fmt --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release --all-features
```

2026-10-20 再次执行上述检查（`cargo test` 为 22 + 10 = 32 个测试通过），并在真实 niri + PipeWire 会话完成验收：`fft-plug` bottom-layer surface、PipeWire active stream、`1920×320` logical surface、Bars/Curve、睡眠唤醒、输出切换与服务重启、缩放与刷新率、10+5 分钟长跑。证据见 `docs/validation/MVP_ACCEPTANCE.md`。

### 仍需真实环境验证或已阻塞

- 多显示器默认行为（当前环境只有单输出）；
- 壁纸轮换与明暗/高对比/高饱和壁纸下的视觉可读性；
- 逐柱毛玻璃：受 GTK 4.24 限制阻塞，当前为透明柱降级；
- 长时间测试按批准缩短为 10 + 5 分钟（计划原定 30 + 10）；
- 不同 compositor 对 `ext-background-effect-v1` 的处理差异（当前 compositor 已实测会拒绝重复对象）。

## 9. 重要设计权衡

### 为什么使用固定 FIFO，而不是无界 channel

频谱是实时视觉数据，过时 PCM 的价值很低。无界队列虽然可以避免丢数据，却会把处理落后转换成越来越大的显示延迟。固定 FIFO 可以把延迟上界暴露出来，并通过 discontinuity 让 DSP 正确恢复。

### 为什么 DSP 和 renderer 都有时间参数，但职责不同

DSP 的 attack/release 表达音频动态风格，是信号处理的一部分；renderer 的短插值只负责在 DSP cadence 和显示刷新 cadence 不同时避免阶梯感。两者不应使用同一组长时间常数重复平滑。

### 为什么 snapshot 只保留最新值

UI 的目标是显示当前声音，而不是回放频谱历史。latest-only 设计避免 UI 卡顿反向阻塞 DSP，也避免在高刷新率下排队大量已经失效的画面。

### 为什么使用 OpenGL，而不是提前迁移 Vulkan/wgpu

当前视觉只有矩形柱、曲线、渐变和少量高光，OpenGL 已能覆盖需求。提前引入第二 renderer 会增加 context、shader、依赖和生命周期复杂度；只有出现明确性能测量或第二后端需求时才应重新评估。

### 为什么逐柱 blur 不写进 OpenGL shader

OpenGL 只能处理 fft-plug 自己的绘制内容，不能直接读取窗口后方的壁纸像素。背景毛玻璃属于 compositor 能力，因此使用 Wayland background-effect 协议提交形状，而不是用前景 shader 模拟背景模糊。

## 10. 后续方向

应按真实需求和测量结果逐项推进：

- 先完成设备切换、PipeWire 重启、context 重建、缩放和长时间性能验证；
- 如果 HSL 在 A/B 中出现明显感知亮度问题，再评估 Oklch/Oklab；
- 指定 PipeWire sink 和多显示器独立 surface；
- waveform、mirror、radial 等新几何；
- glow/bloom/particles 等后处理；
- 配置文件和用户可调参数；
- 只有在出现明确收益后才评估 Vulkan/wgpu 或第二 renderer。

后续工作必须继续遵守当前边界：不破坏 PipeWire realtime callback、不引入无界缓存、不让 UI 反向阻塞 DSP、不把可选视觉效果变成核心音频链路的启动依赖。


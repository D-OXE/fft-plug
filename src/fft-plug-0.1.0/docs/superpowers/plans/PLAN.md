# fft-plug 实施计划

## 1. 目标

将空仓库逐步实现为一个可长期运行的 niri + Wayland 实时音频可视化组件：

```text
PipeWire monitor
    ↓
SPSC PCM ring
    ↓
DSP / FFT
    ↓
latest spectrum snapshot
    ↓
GTK4 layer-shell + GtkGLArea
    ↓
OpenGL Bars / Curve
```

计划的重点不是一次完成所有视觉效果，而是让每个阶段都能独立验证，并始终保持实时路径、资源生命周期和错误恢复可控。

## 当前落地状态

截至 2026-09-15，阶段 0–6 的主链路已经落地：Wayland layer-shell、GtkGLArea/OpenGL、纯 DSP、PipeWire 默认输出 monitor、固定容量有序 PCM FIFO、latest snapshot、Bars/Curve 以及 Silence Mode 均已接入，并通过工程质量检查和纯逻辑测试。本轮又完成了建议中的第一批修正：左右 anchor 协商全宽、默认高度 320 logical px、低频 `--debug` 诊断、FIFO overrun/discontinuity 处理、Sleeping 的最终淡出帧，以及动态观感/空闲唤醒优化。

阶段 7–8 的代码路径已具备基础恢复和关闭处理。2026-10-20 已在真实 niri 26.04 + PipeWire 1.6.9 会话完成验收：真实音频峰值、静音睡眠/唤醒、默认输出切换、PipeWire 重启、输出设备移除后恢复、GL/surface 重建、整数与分数缩放、60/120/160 Hz 刷新率、10 分钟 active + 5 分钟静音长跑均通过；多显示器（当前仅单输出）与壁纸轮换未验证。证据与限制见 `docs/validation/MVP_ACCEPTANCE.md`。建议中提出的 SCTK/EGL/手写 Wayland client 属于后续架构讨论，本阶段按项目约束继续保留 GTK4 + gtk4-layer-shell + GtkGLArea。

本轮动态基线为 `soft = 45/220 ms`、`balanced = 25/150 ms`、`punchy = 15/120 ms`，默认使用 `punchy`。renderer 取消长时二次 attack/release，只保留约 8 ms 的 snapshot 视觉插值；活动和淡出期间继续跟随 GTK frame clock，睡眠且淡出完成后移除 tick callback。DSP worker 无 PCM 时使用事件驱动的 `thread::park()`，PCM 发布和 pipeline 关闭通过无锁 `unpark` 唤醒。transient accent、VBO 预分配和 64/96/128 band A/B 仍等待真实测量，不属于本轮改动。

彩虹流光的 V1/V2 已实现：renderer 自行生成 30 s 周期的连续色相动画，Bars 使用独立的材质透明度、低透明度中性遮罩和顶部高光；不读取壁纸、不接入 niri IPC，也不依赖壁纸管理器。仓库现已提供 `niri/fft-plug.kdl` 作为可直接 include/复制的 niri layer-rule 配置入口；用户仍需手工将它加入自己的 niri 配置，程序不会自动修改 `~/.config/niri`。niri compositor 的 `xray blur` 是可选增强，但实测（2026-10-20）表明**逐柱 blur region 在当前 GTK 4.24 下不可实现**：GTK 自行占用窗口 surface 的 `ext_background_effect_surface_v1`，协议只允许每 surface 一个对象且 GTK 不暴露 region API，原先的客户端实现会让 niri 以协议错误断开连接，因此已移除。当前生效的是透明彩虹柱降级路径；`niri/fft-plug.kdl` 保留但暂不产生可见模糊。

## 2. MVP 完成条件

MVP 必须同时满足：

- niri 中可启动透明 layer-shell surface；
- namespace 固定为 `fft-plug`；
- 默认位于 `bottom` 层，不获取键盘焦点、不申请 exclusive zone；
- 能自动捕获 PipeWire 默认输出 monitor；
- 支持运行时采样率、声道数和格式协商；
- Bars / Curve 能响应频率与响度；
- DSP 结果不会产生传给 renderer 的 NaN/Inf；
- 静音后平滑回落，并进入停止持续重绘的低活动状态；
- 新音频出现后自动恢复；
- PipeWire 重启、默认输出切换、暂时无设备时不 panic；
- realtime path 不阻塞、不无界分配；
- 队列满时延迟不会无限增长；
- 纯 DSP 测试和工程质量检查通过；
- 在真实 niri + PipeWire 环境完成冒烟和长时间运行验证。

## 3. 已确定的技术方向

### 3.1 进程与桌面集成

- 独立 Rust 进程；
- 不修改、不注入 niri；
- GTK4 管理应用生命周期与 Wayland surface；
- `gtk4-layer-shell` 接入 `wlr-layer-shell`；
- layer-shell namespace 固定为 `fft-plug`；
- 默认 `bottom` + bottom/left/right anchor；
- exclusive zone = 0；
- keyboard interactivity = none。

### 3.2 渲染

分成两个阶段：

1. `DrawingArea` 只用于最初的 layer-shell bring-up；
2. 正式实时 renderer 使用 `GtkGLArea + OpenGL + GLSL`。

不把 `DrawingArea/Cairo` 当最终性能路线，也不因为“以后可能需要”提前引入 Vulkan/wgpu。

OpenGL 的目标首先是：

- 透明背景；
- Bars；
- Curve；
- 基础渐变；
- 为未来 glow/bloom/shader 留自然扩展路径。

### 3.3 音频采集

- PipeWire 原生 Rust bindings；
- 自动连接系统默认输出 monitor；
- 优先协商交错 `F32`，但不假定协商一定成功；
- 采样率、声道数、实际格式均来自运行时；
- process callback 只做采样读取、必要轻量转换与 ring buffer 发布；
- callback 中禁止 FFT、绘图、日志、阻塞等待和随时间增长的分配。

### 3.4 实时传输

PipeWire → DSP：

```text
preallocated bounded SPSC ring buffer
```

要求：

- 容量固定；
- producer 不能阻塞；
- 以有序 FIFO 传递 PCM；过载时优先丢弃最旧 READY block，记录 overrun 并让 DSP 在断点后重置分析窗口；
- 暴露 overrun/drop 计数用于诊断；
- 不以 `channel<Vec<f32>>` 作为默认 realtime 方案。

DSP → UI：

```text
latest immutable snapshot
```

不维护待绘制帧队列。

### 3.5 DSP

初始基线：

| 参数 | 初始值 | 说明 |
| --- | --- | --- |
| FFT window | 4096 | 优先保证低频分辨率 |
| hop | 512 | 48 kHz 下约 93.75 Hz 分析更新 |
| window | Hann | 降低泄漏 |
| bands | 64 | 受实际 FFT bin 约束 |
| min frequency | ~20 Hz | 不能低于实际可分辨范围 |
| max frequency | `min(20 kHz, Nyquist)` | 运行时计算 |
| amplitude | dB | 设置 floor |
| dynamics | `soft` / `balanced` / `punchy` | 默认 `punchy`，15/120 ms |

实现必须根据：

```text
bin_width = sample_rate / fft_size
```

预计算 band → bin range，不允许大量 band 重复指向同一单 bin 或产生空 range。

2048 点作为备选，只在视觉响应和延迟测量证明 4096 不合适时采用。

### 3.6 绘制节奏

- DSP 更新率由 hop 决定；
- GPU 绘制跟随 GTK frame clock / 显示器刷新；
- 两者严格解耦；
- DSP attack/release 与睡眠淡出根据真实 `dt` 计算；renderer 只做短视觉插值；
- 不把 60 FPS 或 120 FPS timer 写死为架构假设；
- 活动/淡出时保留 frame-clock tick，睡眠完成后移除 tick，唤醒时重新安装；
- 若需要节能，可增加 renderer FPS ceiling，但它只是策略参数。

### 3.7 Silence Mode

状态：

```text
ACTIVE -> DECAYING -> SLEEPING -> ACTIVE
```

- ACTIVE：正常 DSP + redraw；
- DECAYING：频谱回落并淡出；
- SLEEPING：停止持续 GL redraw、移除 frame tick，并把 FFT 工作降到维持唤醒所需的最低程度；
- 检测到有效音频后恢复 ACTIVE。

静音检测参数必须避免短暂停顿造成频繁睡眠/唤醒。

## 4. 分阶段实施

### 阶段 0：环境与依赖确认

任务：

- 初始化最小 Rust application；
- 确认 Rust stable；
- 确认 PipeWire、GTK4、gtk4-layer-shell、OpenGL/EGL 开发环境；
- 确认 `pkg-config` 能发现系统库；
- 选择兼容的 Rust crate 版本并生成 `Cargo.lock`；
- 确定许可证。

交付物：

- `Cargo.toml`；
- `Cargo.lock`；
- `LICENSE`；
- 与实际发行版匹配的依赖说明。

验证：

```bash
cargo check
```

且不依赖个人绝对路径或临时环境变量。

---

### 阶段 1：layer-shell bring-up

任务：

- 建立 GTK application；
- 创建透明顶层窗口；
- 用 `gtk4-layer-shell` 设置 namespace、layer、anchor、keyboard mode、exclusive zone；
- 先用 `DrawingArea` 画一个静态矩形；
- 验证透明度、HiDPI、关闭流程。

交付物：

- 能在 niri 中稳定出现的静态 surface；
- 最小 `--help`。

验证：

```bash
niri msg layers
```

能够看到 `fft-plug`，且普通窗口布局不受影响。

---

### 阶段 2：OpenGL renderer bring-up

任务：

- 用 `GtkGLArea` 替换实时绘图区域；
- 建立最小 OpenGL context/resource 生命周期；
- 编译简单 vertex/fragment shader；
- 用合成数据绘制固定 bars；
- 再绘制一条静态 curve；
- 正确处理 GL context realize/unrealize；
- 验证透明 framebuffer 与 GTK 合成。

交付物：

- 不依赖 PipeWire/DSP 的 GPU renderer prototype。

验证：

- Resize/HiDPI 后坐标正确；
- 多次打开关闭没有 GL resource leak 或 crash；
- 不使用固定物理像素假设。

---

### 阶段 3：纯 DSP

任务：

- 多声道 interleaved → mono；
- window accumulator + hop；
- 预生成 Hann 系数；
- `realfft`；
- 单边幅值与窗增益归一化；
- dB floor；
- band → bin mapping；
- attack/release；
- 生成不可变 `SpectrumSnapshot`。

首先只使用合成输入：

```text
silence
440 Hz sine
multiple tones
very low amplitude
```

交付物：

- 与 GTK/PipeWire 完全无关的 `dsp` 模块；
- 单元测试。

验证：

- 峰值频率正确；
- 44.1/48/96 kHz 下映射正确；
- 2048/4096 等窗长改变后幅值标尺可解释；
- band ranges 非空且不过度重复；
- 输入为 0 时无 NaN/Inf；
- 平滑不依赖 renderer FPS。

---

### 阶段 4：PipeWire 采集

任务：

- 建立 PipeWire main loop/context/core/stream；
- 查找并连接默认输出 monitor；
- 完成格式协商；
- process callback 读取 buffer；
- 写入预分配、有序 SPSC PCM FIFO；
- 记录轻量 drop/overrun 计数；
- 暂时只输出低频率的采样统计，不连接 UI。

交付物：

- `audio` 模块；
- 可验证的 PCM 数据路径。

验证：

- 播放声音时有非零样本；
- 静音时稳定；
- callback 中无 FFT、阻塞日志和持续分配；
- DSP 故意变慢时不会导致内存增长。

---

### 阶段 5：端到端 Bars

任务：

- PipeWire → SPSC → DSP；
- DSP 发布 latest snapshot；
- GL renderer 从 latest snapshot 读取数据；
- 实现 bars geometry、间距、幅值映射、基础渐变；
- renderer 跟随 frame clock；
- 记录 frame age / drop 指标。

交付物：

```bash
fft-plug --mode bars
```

验证：

- 单频测试音的主峰在正确区域；
- 系统音量变化会反映到视觉幅值；
- UI/DSP 变慢不阻塞 PipeWire callback；
- 长时间运行内存稳定。

---

### 阶段 6：Curve + Silence Mode

任务：

- Curve 复用相同 `SpectrumSnapshot`；
- 构造平滑但受限的曲线几何；
- 避免 spline overshoot 导致负值或越界；
- 实现 ACTIVE / DECAYING / SLEEPING；
- SLEEPING 时停止持续 redraw；
- 新音频出现时可靠唤醒。

交付物：

```bash
fft-plug --mode curve
```

以及可用的 silence behavior。

验证：

- Bars/Curve 峰值位置一致；
- 静音若干秒后 GPU redraw 停止；
- 音频恢复后不会漏唤醒；
- 无 PCM 时 DSP worker 不进行周期性 timeout polling；
- 快速暂停/恢复不会造成闪烁或状态抖动。

---

### 阶段 7：恢复能力与桌面边界

任务：

- 默认输出切换；
- PipeWire 服务重启；
- 格式/采样率改变；
- 输出设备休眠/唤醒；
- GL context 重建；
- GTK 正常关闭；
- 分数缩放；
- 至少一次多显示器行为确认，即使 MVP 尚不提供显示器选择参数。

交付物：

- 可长时间运行的 MVP。

验证：

- 扬声器/耳机切换后恢复；
- 重启 PipeWire 后恢复或明确提示可重试错误；
- 多次启动/关闭没有残留线程；
- context 重建不崩溃；
- 无持续日志刷屏。

---

### 阶段 8：文档、质量与发布准备

任务：

- README 从“计划”更新为实际行为；
- 写明安装依赖、构建、运行、niri 配置；
- 记录多显示器/设备选择等已知限制；
- 检查实际 diff；
- 完成真实环境冒烟与长时间测试。

验证：

```bash
cargo fmt --check
cargo check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

## 5. 建议提交拆分

如果需要创建 Git 历史，建议：

1. 初始化 Rust 工程、许可证和基础文档；
2. layer-shell 静态 surface；
3. GtkGLArea/OpenGL prototype；
4. 纯 DSP 与测试；
5. PipeWire + SPSC 采集；
6. 端到端 Bars；
7. Curve + Silence Mode；
8. 输出切换、恢复和生命周期；
9. 文档与发布准备。

除非明确要求，不自动创建 commit、branch 或 push。

## 6. 风险与对策

| 风险 | 影响 | 对策 |
| --- | --- | --- |
| GTK + GLArea + layer-shell 组合存在集成问题 | GPU 路线受阻 | 在阶段 2 早期单独验证，不拖到端到端阶段 |
| PipeWire callback 分配/阻塞 | 丢块、卡顿 | 预分配 SPSC、专项审查 realtime path |
| 默认 monitor 选择差异 | 采不到正确音频 | 先验证会话管理器行为，再监听默认节点变化 |
| 4096 FFT 视觉响应偏软 | 动画迟钝 | 保持 hop=512，实测后比较 2048 |
| 对数 bands 低频重复 | 低频柱无意义 | 基于 bin_width 生成唯一有效 band ranges |
| 平滑依赖 FPS | 显示器刷新率不同导致动画变化 | 时间常数 + dt |
| GPU redraw 静音时持续运行 | 空闲功耗 | Silence Mode 停止持续 redraw |
| GL context 生命周期错误 | 关闭/恢复 crash | 所有 GL 资源严格绑定 realize/unrealize |
| 分数缩放坐标错误 | 模糊、尺寸偏差 | 只使用逻辑尺寸，真实 niri 缩放测试 |
| 系统库/crate 不兼容 | 构建失败 | 阶段 0 锁定兼容组合并提交 Cargo.lock |

## 7. 暂缓项

MVP 完成前不实现：

- 完整配置 GUI；
- 用户 shader；
- Vulkan/wgpu renderer；
- MPRIS 封面/歌词；
- wallpaper/album 自动取色；
- 多 pass bloom、particles、radial；
- 多 renderer 抽象；
- X11 后端；
- niri IPC 集成；
- systemd user service 与发行版打包。

这些能力只有在真实需求出现后再进入架构。

## 8. 彩虹流光与毛玻璃视觉升级计划

### 8.1 状态与目标

状态：**V1/V2 已实现并通过真实桌面验收；V3 受 GTK 4.24 限制阻塞（透明柱降级生效）；V0 部分完成（截图与 ACTIVE/SLEEPING 资源数据已记录），V4 的长跑与切换验证已完成，多显示器/壁纸轮换未验证**。

目标是在不读取壁纸路径、像素或主题色的前提下，为 Bars 增加缓慢横向流动的连续彩虹色环，并利用 niri 的 compositor 背景效果形成毛玻璃观感。壁纸定时替换后，fft-plug 无需重新取色或重启。

目标视觉层次：

```text
当前壁纸
    ↓ niri xray + ext-background-effect-v1
每根柱子对应的局部毛玻璃区域（柱间保持清晰）
    ↓ 低透明度中性遮罩
半透明彩虹柱
    ↓
顶部高光 / 轻量边缘强调
```

功能边界：

- Bars 是第一验收对象；
- Curve 必须保持现有行为和频谱语义，不得因 Bars 视觉升级退化；
- 彩虹色由 renderer 程序化生成，不读取 DMS、Matugen、壁纸文件或屏幕截图；
- 毛玻璃由 niri compositor 完成，OpenGL 不尝试读取窗口后方像素；
- blur region 由 renderer 按每根柱子的实际高度逐帧提交，柱间空隙不加入区域；
- 不增加第二 renderer，不改动 PipeWire、DSP 或 snapshot 契约；
- 不在第一版加入配置 GUI、多 pass bloom、particles 或用户 shader。

### 8.2 视觉参数基线

以下数值是首轮真实桌面调试起点，不是稳定公开契约：

| 参数 | 初始值 | 约束 |
| --- | --- | --- |
| 色环空间跨度 | 屏幕宽度覆盖 1 个完整色环 | 相邻柱颜色连续，不做离散随机色 |
| 完整轮回周期 | 30 s | 使用真实 `dt`，不能绑定刷新率 |
| 彩虹饱和度 | 0.72 | 避免高饱和 RGB 灯带感 |
| 彩虹亮度 | 0.62 | 保持不同色相的基本可读性 |
| 柱体材质 alpha | 0.45 | 与活动淡入/淡出 alpha 分离 |
| 顶部高光 alpha | 0.68 | 只强调轮廓，不扩散成 bloom |

颜色模型第一版使用受限 HSL/HSV，并对最终 RGB 执行 finite 与 `[0, 1]` clamp。若真实 A/B 明确显示黄色过亮、蓝色过暗，再将颜色生成替换为固定 lightness/chroma 的 Oklch/Oklab 色环；不在没有视觉证据时先增加这部分复杂度。

色相定义：

```text
phase += dt / cycle_seconds
hue(x, phase) = fract(x_normalized + phase)
```

- `phase` 必须有界回绕，长时间运行不能持续增大；
- 同一柱子可以通过左右顶点的相邻 hue 插值得到连续横向渐变；
- 纵向只调整明暗和高光，不在单根柱子内部再重复完整色环；
- 音频幅值只控制高度，不参与 hue 速度，避免响度变化导致颜色跳变。

### 8.3 动画与 Silence Mode

彩虹动画必须复用现有 GTK frame-clock 路径：

- ACTIVE：按真实 `dt` 推进色相并绘制；
- DECAYING：允许色相继续缓慢移动，同时完成柱高和可见度回落；
- SLEEPING：冻结彩虹 phase，完成淡出后返回“不再继续绘制”，让现有 tick callback 被移除；
- SLEEPING → ACTIVE：从冻结的 phase 继续，不因 wall clock 产生大幅颜色跳变；
- 不新增固定 60/120 FPS timer；
- 不为颜色动画重复执行 FFT，也不改变 DSP cadence。

需要把当前单一 `display_alpha` 的两个含义拆开：

```text
final_alpha = lifecycle_alpha × material_alpha
```

- `lifecycle_alpha` 只负责 ACTIVE/DECAYING/SLEEPING 的淡入淡出；
- `material_alpha` 表示玻璃柱自身的透明度；
- 调低玻璃透明度不能改变睡眠完成判定。

### 8.4 毛玻璃集成

第一版使用 niri 26.04 的 `ext-background-effect-v1` 与 layer background effect。仓库提供 `niri/fft-plug.kdl` 配置片段，用户可以直接 include 或复制到自己的 niri 配置；程序不自动修改用户的 niri 配置：

```kdl
layer-rule {
    match namespace="^fft-plug$"

    background-effect {
        xray true
    }
}
```

选择 `xray true` 的理由：

- 频谱位于 `bottom` layer，主要视觉背景就是壁纸；
- 壁纸轮换时由 compositor 更新模糊背景，fft-plug 无需获知壁纸来源；
- xray 模糊可以复用壁纸背景，比实时模糊整个桌面内容更符合常驻组件的功耗目标；
- 普通窗口覆盖频谱仍是既定行为。

设计意图是程序在每个 Bars 帧把当前可见柱体转换为 surface-local `wl_region`，通过 `ext-background-effect-v1` 交给 niri；协议规定该区域在下一次 `wl_surface.commit` 时生效，区域之外不会产生背景模糊。不要再为 fft-plug 配置 `blur true`，因为 niri 规则自身只知道整层矩形，会把柱间空隙也模糊掉。

**2026-10-20 实测结论：该路径在当前 GTK 4.24 下不可实现，客户端实现已移除。** GTK 在创建窗口 surface 时自行 `get_background_effect(wl_surface)`，`ext-background-effect-v1` 规定每个 `wl_surface` 最多一个 `ext_background_effect_surface_v1`，重复创建会让 niri 发送 `wl_display.error` 并断开连接（实测启动即 `Gdk-Message: Error 71 (协议错误)`）。GTK 也未公开设置 blur region 的 API，且从不调用 `set_blur_region`。因此当前只有透明彩虹柱降级路径；若将来 GTK 暴露 region 控制，可恢复逐柱 region 提交。

降级规则：

- niri 未启用 blur、compositor 不支持对应效果或用户不添加 layer rule 时，程序仍显示透明彩虹柱；
- 启动不得因为缺少 blur capability 失败；
- `ext-background-effect-v1` 不可用时不影响启动，透明彩虹柱作为正式降级路径。

`niri/fft-plug.kdl` 只包含 `namespace="^fft-plug$"` 的精确匹配和 `background-effect { xray true }`，不包含全局 blur、其他窗口规则或固定整层 blur 配置。

如果柱体在真实桌面上对比度不足，优先调整柱体材质 alpha 与顶部高光；不要修改全局 niri blur 参数而影响其他程序。

### 8.5 分阶段实施

#### 阶段 V0：建立视觉基线

当前记录（2026-10-20）：niri 26.04 / PipeWire 1.6.9 / DP-2 3840x2160@160 Hz、scale 2。已建立可截图的 layer surface：Bars 与 Curve 截图、ACTIVE（CPU 17–19%）与 SLEEPING（CPU 0.1%、render 冻结）资源数据均已记录；用户配置中已存在 `^fft-plug$` layer-rule，因此“无 rule”对照未执行。

任务：

- 记录当前 Bars 在明亮、暗色和高饱和壁纸下的截图；
- 记录 ACTIVE 与 SLEEPING 的 render frame 计数和 CPU 使用率；
- 确认当前 niri compositor 版本与 `background-effect` 支持；
- 确认没有 fft-plug 专属 blur rule 时的现状。

验收：

- 有可以与改动后对照的截图和空闲资源数据；
- 不修改音频、DSP 或用户 niri 配置。

#### 阶段 V1：纯颜色与相位逻辑

状态：**已完成（代码与纯逻辑测试）**。

任务：

- 增加有界 rainbow phase；
- 增加 hue → RGB 转换与颜色 clamp；
- 将颜色计算保持为纯函数，不依赖 GTK、PipeWire 或系统时间；
- 为不同 `dt` 分割下的等价相位、phase 回绕、finite 输出和颜色范围添加测试。

验收：

- 30 秒累计结果不因 60/120/160 Hz 的帧分割产生可见速度差异；
- 极端或非有限输入不能把 NaN/Inf 传给 vertex buffer；
- 纯逻辑测试不要求 Wayland/OpenGL 会话。

#### 阶段 V2：Bars 彩虹流光

状态：**已完成（代码与纯逻辑测试）**。

任务：

- 用横向位置与 phase 替换 Bars 的固定蓝粉渐变；
- 分离 lifecycle alpha 与 material alpha；
- 保持现有 band、高度、间距和 snapshot 插值语义；
- 增加顶部高光所需的最少几何，不引入 framebuffer/post-process pass；
- 确保 Curve 行为不变。

验收：

- 整个宽度构成连续色环且平滑流动；
- 音频峰值位置和柱高与修改前一致；
- 不出现颜色跳变、明显接缝、负 alpha 或非有限顶点数据；
- SLEEPING 后 phase 和持续 redraw 都停止，恢复音频后正常继续。

#### 阶段 V3：niri 毛玻璃与视觉调参（受环境阻塞）

状态：**未完成，且在当前 GTK 4.24 下不可实现**（原因见 8.4）。以下任务保留为将来 GTK 支持 region 控制后的清单。

任务：

- 使用仓库提供的 `niri/fft-plug.kdl`，或复制其内容，手工增加 fft-plug 专属 layer rule 进行冒烟，不由程序写入配置；
- 验证 `xray true + ext-background-effect-v1` 在定时换壁纸前后的行为；
- 调整材质 alpha、中性遮罩、顶部高光、饱和度与轮回周期；
- 分别检查亮色、暗色、高对比和高饱和壁纸；
- 将确认后的 niri 配置入口、逐柱 blur region 和降级行为写入 README。

验收：

- 更换壁纸后无需重启 fft-plug，毛玻璃背景随 compositor 更新；
- 不读取壁纸路径、屏幕像素或主题缓存；
- 关闭 blur rule 后仍能正常显示和退出；
- 柱间空隙和没有柱体的区域保持清晰，不形成整块桌面底板。

#### 阶段 V4：质量、性能与回归

执行：

```bash
cargo fmt --check
cargo check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

桌面验证至少覆盖：

- Bars 对真实音乐持续响应；
- Curve 未被破坏；
- 60/120/160 Hz 或可获得的不同刷新环境下颜色周期接近一致；
- 壁纸轮换期间没有闪黑、残留旧背景或程序崩溃；
- 静音淡出、睡眠、自动唤醒保持正常；
- 比较修改前后的 CPU、render frame 数和稳定内存；
- GL context 重建后 phase、shader 和资源状态有效；
- 整数/分数缩放下没有颜色接缝或高光错位。

### 8.6 完成条件

当前 V1/V2 已在真实桌面验证；长时间性能数据已获得；V3 因 GTK 4.24 独占 surface background-effect 对象而阻塞，降级路径（透明彩虹柱）生效；多显示器与壁纸轮换未验证。因此本节完成条件中“niri blur 可选增强”一条按“缺失时正常降级”满足，“逐柱毛玻璃呈现”一条不满足。

本轮视觉升级只有在以下条件全部满足时才能标记完成：

- Bars 使用连续、缓慢、与 FPS 无关的全色相流动；
- 彩虹颜色无需壁纸、DMS、Matugen、截图或 niri IPC；
- niri blur 是可选增强，缺失时程序可正常降级；
- 壁纸定时替换不要求重启 fft-plug；
- lifecycle alpha 与材质 alpha 分离，静音淡出语义不变；
- SLEEPING 仍会停止 frame tick 和持续 GL redraw；
- PipeWire callback、DSP、pipeline 和 `SpectrumSnapshot` 契约没有变化；
- Bars/Curve 的峰值位置与幅值语义没有回归；
- 工程质量检查与相关测试通过；
- README 只在功能真实实现并验证后更新为“已实现”。

### 8.7 风险与对策

| 风险 | 影响 | 对策 |
| --- | --- | --- |
| 彩虹动画过快或过饱和 | 视觉疲劳、抢占桌面注意力 | 30 s 周期、限制饱和度和亮度，真实 A/B 后再调整 |
| HSL/HSV 不同色相感知亮度不一致 | 黄色过亮、蓝色过暗 | 先限制 lightness/value；有明确证据后再改 Oklch/Oklab |
| niri 规则误用 `blur true` | 形成整层矩形底板 | 仅保留 `xray true`；逐柱 region 当前因 GTK 4.24 占用 effect 对象而不可用，保持无 blur 降级 |
| 亮色或复杂壁纸降低柱体对比度 | 柱体难以辨认 | 中性遮罩、顶部高光和有限的边缘强调 |
| 彩虹动画阻止睡眠 | 空闲 GPU/CPU 占用回升 | phase 服从 ActivityState；SLEEPING 冻结并移除 tick |
| 逐帧临时分配增加 | 高频刷新时产生额外开销 | 复用现有 vertex buffer，颜色计算不得创建每帧容器 |
| compositor 不支持 blur | 毛玻璃缺失 | 透明彩虹柱作为正式降级路径，不能启动失败 |
| 修改全局 blur 参数 | 影响桌面其他程序 | 只增加 namespace 精确匹配的 layer rule，不自动改全局设置 |

### 8.8 明确暂不实施

- 壁纸/专辑自动取色；
- 屏幕截图、screencast 或逐像素背景采样；
- niri IPC；
- Gaussian blur 写入 fft-plug shader 来冒充背景模糊；
- 多 pass bloom、离屏 framebuffer、粒子系统；
- 颜色、速度和玻璃参数的配置 GUI；
- 第二 renderer 或 Vulkan/wgpu 迁移。

## 9. 实施原则

- 每阶段必须有单独可验证结果；
- 优先验证高风险集成点，而不是先写大量业务代码；
- UI、DSP、音频采集都必须能单独验证；
- realtime path 优先保证边界和时效性；
- 不用无界缓存换“看起来不卡”；
- 不在缺少测量时重写后端；
- 不为了未来可能存在的第二实现提前抽象 trait；
- 最终文档必须描述真实代码，而不是设计愿望。

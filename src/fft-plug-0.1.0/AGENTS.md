# fft-plug 项目约束

本文件定义 `fft-plug` 仓库长期有效的工程约束。实现任务应以 `README.md` 的产品边界和 `PLAN.md` 的当前阶段为准；如果子目录以后出现更具体的 `AGENTS.md` / `AGENTS.override.md`，以更具体规则为准。

## 1. 产品边界

- `fft-plug` 是独立 Wayland 客户端，不修改、不注入 niri。
- 桌面集成使用 `wlr-layer-shell`。
- 不为旧环境增加 X11/XWayland 专用实现。
- MVP 音频源只考虑 PipeWire 系统默认输出 monitor。
- 默认不保存 PCM、不保存频谱、不联网。
- MVP 只完成“采集 → DSP → GPU 显示 → 静音睡眠 → 基础恢复”。
- 不提前建设插件系统、设置中心、主题市场、MPRIS、歌词或用户 shader。

## 2. 技术边界

默认技术选择：

- Rust stable；
- PipeWire 原生 Rust bindings；
- `realfft` / `rustfft`；
- GTK 4；
- `gtk4-layer-shell`；
- `GtkGLArea + OpenGL + GLSL` 作为正式 renderer。

`DrawingArea/Cairo` 仅允许用于早期 layer-shell 静态 bring-up 或调试，不应自然演变成最终实时 renderer。

不要在没有明确需求或测量证据时：

- 改用 Vulkan/wgpu；
- 自己重写完整 Wayland client stack；
- 添加第二个 renderer；
- 为未来后端提前设计公共 trait 层。

## 3. 模块边界

优先保持：

```text
src/
├── main.rs
├── audio/
├── dsp/
├── pipeline/
├── shell/
└── render/
```

职责：

- `audio/`：PipeWire 对象发现、默认输出、格式协商、采样读取；
- `dsp/`：混音、窗函数、FFT、归一化、dB、频段聚合、平滑；
- `pipeline/`：SPSC PCM ring、latest snapshot、跨线程状态协调；
- `shell/`：GTK application、layer-shell、GLArea 生命周期、frame clock；
- `render/`：OpenGL resource、shader、geometry、Bars/Curve；
- `main.rs`：CLI、装配、启动/关闭，不放具体 DSP 或 GL 算法。

若模块过小，应合并，不要为了符合目录图制造空文件和空抽象。

纯 DSP 不得依赖 GTK、Wayland、PipeWire 或 OpenGL。

## 4. PipeWire realtime 约束

PipeWire process callback 是实时敏感路径：

- 不执行 FFT；
- 不做频段聚合；
- 不绘图；
- 不阻塞；
- 不等待普通 mutex；
- 不进行文件/终端 I/O；
- 不输出逐块日志；
- 不 panic；
- 不做随输入持续增长的 heap allocation；
- 不构造每块一个新的 `Vec<f32>` 作为默认传输模型。

callback 只做：

1. 读取协商后的 buffer；
2. 必要的轻量格式/声道处理；
3. 写入预分配、有界的 SPSC PCM ring；
4. 更新低成本计数器。

producer 绝不能因为 DSP 落后而阻塞。

## 5. 实时传输约束

### Audio → DSP

- 使用容量固定的 SPSC ring buffer；
- 容量在 stream 建立/格式变化时准备；
- buffer 满时必须有明确策略；
- 默认“最新声音优先”：允许丢弃旧 PCM，不允许无限积压；
- drop/overrun 需要可观测计数。

### DSP → UI

- 只暴露最近一次完整的不可变 `SpectrumSnapshot`；
- 不排队历史 spectrum frame；
- UI 慢不能反向阻塞 DSP；
- snapshot 的生命周期必须清晰，不依赖悬空引用或跨线程 GTK 对象。

## 6. DSP 不变量

- DSP 对确定输入产生确定输出；
- 不直接读取环境变量、桌面状态或 GTK frame clock；
- 默认 Hann window；
- FFT 幅值必须考虑窗长和窗增益进行一致归一化；
- 单边 spectrum 的 DC/Nyquist 特殊情况需要正确处理；
- 频率坐标只由运行时 `sample_rate` 与 `fft_size` 推导；
- 显示上限不得超过 Nyquist；
- dB 转换必须对 0/极小值设置 floor；
- 禁止向 renderer 输出 NaN/Inf；
- attack/release 使用时间尺度定义，不能绑定固定 FPS；
- Bars 和 Curve 共享同一频谱数据语义，不复制两套 DSP；
- band mapping 必须基于真实 FFT bin 分辨率，不制造大量空 band 或重复单 bin band。

当前基线为 4096 window + 512 hop；改变默认值时必须同步测试和 README，并说明测量依据。

## 7. Renderer 约束

- GTK 管 surface；OpenGL 管实时视觉；
- GL 资源只在有效 context 内创建/销毁；
- 正确处理 `GtkGLArea` realize/unrealize；
- 不假定固定屏幕尺寸、固定 DPI 或固定刷新率；
- 坐标使用逻辑尺寸并验证分数缩放；
- renderer 读取 latest snapshot，不持有 PipeWire 或 DSP 内部对象；
- shader compile/link 错误必须在启动阶段给出可操作信息；
- 不在每帧创建大量临时 heap 对象；
- 曲线插值需要约束 overshoot，禁止负高度与非有限坐标。

首次实现以简单 Bars/Curve 为目标。Glow、Bloom、Particles、Radial 等不得提前侵入基础 renderer API。

## 8. 绘制节奏

- DSP cadence 与 render cadence 解耦；
- render 优先跟随 GTK frame clock；
- 不用固定 60/120 FPS timer 作为核心模型；
- 如果以后提供 FPS ceiling，它只是节能策略；
- 动画参数以真实 `dt` 计算；
- 不为了高刷新率重复执行 FFT。

## 9. Silence Mode

MVP 必须具备：

```text
ACTIVE -> DECAYING -> SLEEPING -> ACTIVE
```

约束：

- DECAYING 完成平滑回落和淡出；
- SLEEPING 停止持续 GL redraw；
- SLEEPING 不得通过“完全停止采集”导致无法自动唤醒；
- 唤醒路径必须轻量且可靠；
- 短暂停顿不能造成频繁 sleep/wake 抖动；
- silence threshold / hold time / fade time 的默认值必须通过真实音频测试调整。

## 10. 生命周期与错误恢复

组件所有权必须清晰：

```text
Audio produces PCM
DSP produces SpectrumSnapshot
UI consumes snapshot
```

正常关闭顺序：

1. 停止接收新采样；
2. 唤醒并终止 DSP worker；
3. 停止 renderer/frame callbacks；
4. 销毁 GL resources/context-associated objects；
5. 销毁 GTK/PipeWire 资源。

以下情况是可恢复状态，不应直接 panic：

- 默认输出变化；
- PipeWire 服务重启；
- 暂时没有输出设备；
- 采样率/声道/格式变化；
- 设备休眠与恢复。

重复错误需要限速或去重，禁止刷屏。

## 11. Wayland / niri 约束

- namespace 固定为 `fft-plug`；
- 默认 layer = `bottom`；
- exclusive zone = 0；
- keyboard interactivity = none；
- 默认锚定 bottom + left + right；
- layer、anchor、尺寸、输入区域集中定义，不散落魔法值；
- 普通窗口覆盖频谱是预期行为；
- 多显示器不能硬编码 connector 名或屏幕像素尺寸；
- niri 特定行为优先用公开 layer-shell 和配置解决；
- 没有明确需求时不接入 niri IPC。

## 12. unsafe / FFI

- 不用 `unsafe` 绕过 Rust 所有权或线程边界；
- PipeWire/OpenGL FFI 需要 `unsafe` 时尽量缩小作用域；
- 每个非显然的 `unsafe` 块必须说明安全前提；
- FFI 指针不得跨越其底层对象生命周期；
- 不手工修改 generated bindings 或构建产物来“修编译”。

## 13. 测试

纯 DSP 测试至少覆盖：

- silence；
- 440 Hz 单频；
- 多频叠加；
- 极低幅值；
- 多声道混合；
- 44.1 / 48 / 96 kHz；
- 2048 / 4096 等不同窗长；
- band mapping 非空、合法、单调；
- dB floor / finite output；
- attack/release 上升与回落；
- 状态重建边界情况。

默认单元测试不得要求：

- 真实 PipeWire daemon；
- 真实音频设备；
- Wayland session；
- OpenGL desktop session。

需要这些环境的测试标记为 integration/manual smoke test。

## 14. 性能验证

先测量再优化。至少观察：

- PipeWire overrun/drop；
- PCM ring occupancy；
- DSP snapshot age；
- render frame age；
- CPU 使用率；
- 静音状态 GPU redraw 是否停止；
- 稳定内存占用。

性能优化不得：

- 用无界缓存掩盖处理不过来；
- 破坏峰值频率正确性；
- 破坏输出切换恢复；
- 只凭 benchmark 数字牺牲视觉时效性。

## 15. 工程质量

相关修改完成后执行：

```bash
cargo fmt --check
cargo check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

无法执行某项检查时必须说明真实原因，不伪造结果。

涉及以下内容的修改在结束前必须审查实际 diff：

- realtime callback；
- FFI / unsafe；
- 线程与关闭流程；
- GL resource 生命周期；
- CLI；
- 公共配置；
- 性能关键路径；
- 多文件重构。

## 16. 文档同步

以下变化必须同步 README / PLAN：

- MVP 用户可见行为；
- 系统依赖；
- CLI；
- layer-shell namespace / layer 行为；
- FFT window / hop / band 规则；
- renderer 后端；
- Silence Mode；
- 已知限制与已完成阶段。

未实现的能力必须明确写成“计划”“草案”或“后续”，不能描述成已经存在。

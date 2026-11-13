# fft-plug MVP Acceptance

本文件只记录本轮（2026-10-20，Asia/Shanghai）实际执行过的命令和桌面证据。
历史记录、推断和“应该可以”的内容一律不写入通过项；未执行的章节标记 `Not run`。

## Environment

<!-- AI Fix - 2026-11-13 13:51 - 泛化本机设备、connector 和用户配置路径 -->
| 项 | 值 | 来源 |
| --- | --- | --- |
| rustc | 1.99.1 (58cf8f9c3 2026-10-13, Arch Linux rust 1:1.99.1-1) | `rustc --version` |
| cargo | 1.99.1 (5f94df478 2026-08-27) | `cargo --version` |
| gtk4 | 4.24.1 | `pkg-config --modversion gtk4` |
| libpipewire-0.3 | 1.6.9 | `pkg-config --modversion libpipewire-0.3` |
| niri | 26.04 (8ed0da4) | `niri --version` |
| PipeWire 服务 | `pipewire-0` 1.6.9 | `wpctl status` |
| 桌面会话 | `XDG_CURRENT_DESKTOP=niri`，`WAYLAND_DISPLAY=wayland-1` | 环境变量 |
| 输出 | 单个高刷新率 HiDPI 输出（connector、型号和精确分辨率省略） | `niri msg outputs` |
| 可用 sink | 多个本地输出设备（名称和型号省略） | `wpctl status` |
| niri layer-rule | 用户 niri 配置中已有 `^fft-plug$` + `xray true` | 已存在于用户配置 |
| 源码状态 | commit `3137323` + 本轮未提交工作区改动 | `git log` / `git status --short` |

## Build and Unit Checks

本轮实际执行（release 构建用于所有桌面测量）：

```bash
cargo fmt --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo test --no-default-features
cargo build --release --all-features
```

结果：全部 exit 0。`--all-features` 为 22（lib）+ 10（bin）= 32 个测试通过；
`--no-default-features` 为 22 通过 + 0 + 0。

数量变化说明：Task 2 新增 3 个诊断测试（32→35）；删除 `render/background_effect.rs`
（见 Failures and Fixes）去掉 3 个 blur geometry 测试（35→32）。

## Audio and Recovery

测试音：48 kHz / 16-bit / 2 ch，440 Hz 正弦，幅度 0.35（`pw-play`）。
长时间负载：`python3` 生成 10 分钟混合信号（62 Hz 鼓点、宽带噪声、三音和弦、1 s 停顿、
100 Hz→5 kHz 扫频、每 10 s 变化音量），经 `pw-cat --playback --raw --format s16 --rate 48000 --channels 2 -` 播放。

| 场景 | 结果 |
| --- | --- |
| 启动 | `audio.state=Connected rate=48000 channels=2 format=283`，`session_generation=1 reconnects=0` |
| 默认 512-frame quantum | callbacks 94/s、DSP snapshot 93/s、render 160/s、PCM ~48 k frames/s |
| 440 Hz 测试音 | `peak=0.350`（与生成幅度一致），`dsp.last_peak_hz=404.1`，`snapshot_age_ms` 2–5，`render.frame_age_ms` 3 |
| 静音 | 见下方 Silence |
| 默认输出切换 | `wpctl set-default <sink-id>` 后 fft-plug 采集链接移到新默认设备的 monitor；峰值非零；`sessions` 仍为 1（stream 跟随默认节点，无需重连）；切回原默认设备后采集正常恢复 |
| PipeWire 重启 | `systemctl --user restart pipewire` → `audio.sessions=2 audio.reconnects=1`，随后 `Connected`，callbacks/DSP 恢复 |
| 全部输出设备移除 | 三张声卡 profile 置 `off` → WirePlumber 建立 `虚拟输出` 并设为默认；fft-plug 保持 `Connected` 且存活，callbacks 继续增长，无 panic、无日志刷屏；恢复 profile 后 `peak=0.350` |
| 唤醒延迟 | 静音 ≥200 s 后播放测试音，下一次 1 s 诊断行即为 `activity=Active`、`render.frame_age_ms=1`；内部唤醒轮询为 80 ms |

关于“snapshot 频率”：`Analyzer::process` 对每个 block 内所有完整 hop 都做 FFT，但只发布最新一个
snapshot。默认 512-frame quantum 下 callbacks 与 snapshot 都是 ~94/s；当另一客户端把 graph
quantum 拉到 2048 frames 时，callbacks 与发布频率为 24/s（每 block 一次），PCM 仍为 48 k frames/s，
FFT 仍按 hop 执行。这是 “latest-only snapshot” 契约的结果，不是丢样本：`pcm.overruns=0`、
`pcm.queued=0`、`pcm.written==pcm.consumed`。

`dsp.last_peak_hz` 取的是 band 中心频率，而 band 边界按 `position^1.35` 分布（近似线性而非对数），
因此 440 Hz 落在 band index 3（中心 404.1 Hz，屏幕左侧约 5% 处）。这只是纯音测试时看起来
“只有左边几根柱子”的原因；DSP 单元测试覆盖了 440 Hz 峰值落在 300–650 Hz band 的断言。

## Layer Shell and OpenGL

| 项 | 证据 |
| --- | --- |
| layer / namespace / 键盘 | `niri msg layers`：`Bottom layer` → `Namespace: "fft-plug"`、`Keyboard interactivity: none` |
| anchor / exclusive zone | 代码固定 left+right+bottom、zone 0；截图中 surface 占满底部 320 logical px，普通窗口仍可覆盖该层，桌面布局未预留空间 |
| Bars | 白噪声下 64 根柱子满宽显示，彩虹渐变连续、柱间透明、顶部高光可见（`/tmp/final-bars.png`） |
| Curve | `--mode curve` 正常绘制 LINE_STRIP，无 NaN、黑屏或 shader 错误（`/tmp/final-curve.png`） |
| 刷新率 | 在测试输出上临时切换 120 Hz / 60 Hz / 160 Hz：`render.frames` 增量分别为 120/60/160 每秒，之后恢复原模式 |
| 分数缩放 | 临时切换 scale 1.5 后截图满宽、无接缝、无裁剪；之后恢复原 scale |
| GL / surface 重建 | 将测试输出 off 再 on：WAYLAND trace 出现第二组 `wl_compositor.create_surface` + `zwlr_layer_shell_v1.get_layer_surface`，之后 `render.frames` 继续增长、`frame_age_ms` 3–6，说明 GL/context 资源随新 surface 重建 |
| 多显示器 | `Not run`：当前只有一个输出，无法验证多显示器默认行为 |

## Visual and Background Effect

- 用户配置中已启用 `layer-rule { match namespace="^fft-plug$" background-effect { xray true } }`，
  本轮未修改 `~/.config/niri`。
- **逐柱毛玻璃未通过，且在当前 GTK 下不可实现。** 证据：
  - `WAYLAND_DEBUG=1` trace 显示 GTK 4.24 在创建窗口 surface 时自行
    `wl_registry.bind(ext_background_effect_manager_v1)` 并
    `get_background_effect(wl_surface)`；一个 5 行纯 PyGObject 窗口程序同样触发该请求（与 fft-plug 无关）。
  - 原 `src/render/background_effect.rs` 对同一 `wl_surface` 再创建一次该对象，niri 返回
    `wl_display.error(..., "wl_surface already has a background effect object attached")`，
    GTK 报 `Error 71 (协议错误)` 并退出。修复前程序在 niri 上无法启动。
  - 协议规定每个 `wl_surface` 只能有一个 `ext_background_effect_surface_v1`。GTK 未暴露设置
    blur region 的公开 API，且修复后的 trace 中 GTK 从不调用 `set_blur_region`。
- 因此生效的是计划中的正式降级路径：**透明彩虹柱**（无 blur）。启用 niri rule 后画面也不出现
  整层模糊底板，因为没有任何客户端提交 region。截图 `/tmp/final-bars.png`、`/tmp/crop-scale15.png`。
- 壁纸轮换：`Not run`（壁纸由本地桌面组件管理，自动轮换不可安全触发）。
- 明暗/高对比/高饱和壁纸对照：`Not run`（不修改用户壁纸）。

## Performance and Long Run

环境：release 构建，Bars + punchy，160 Hz / scale 2，niri rule 存在但无 blur。采样：每 30 s 读
`/proc/<pid>/stat` 与 `VmRSS`；每 1 s 的 `--debug` 行提供 callbacks/PCM/DSP/render 指标。

### Active（10 分钟混合负载，用户批准的缩短方案；计划原定 30 分钟）

| 时间点 | CPU | RSS | 备注 |
| --- | --- | --- | --- |
| 0 min | 17.4% | 127,868 kB | 启动预热后 |
| 2 min | 17.6% | 127,868 kB | |
| 4 min | 17.1% | 128,040 kB | |
| 6 min | 18.5% | 128,040 kB | |
| 8 min | 18.1% | 128,040 kB | |
| 10 min | 18.0% | 128,040 kB | 播放结束 |

- `pcm.overruns=0`、`drops=0`、`unsupported=0` 全程；`pcm.written == pcm.consumed`。
- `render.frame_age_ms` 2–5 ms（160 Hz 下每帧 6.25 ms 预算）；DSP `snapshot_age_ms` 稳态 2–30 ms，
  Active 行中的最大 469 ms 出现在短暂 Sleeping→Active 切换后的第一行（旧 snapshot 的年龄），
  下一次发布即恢复。
- 无 crash、无重复错误日志。RSS 在 4 分钟内上升 172 kB 后完全持平（分配器预热）。

### Silent（5 分钟，用户批准的缩短方案；计划原定 10 分钟）

- 播放停止后约 0.55 s 进入 `DECAYING`，随后 `Sleeping`；`render.frames` 冻结在 94,705 长达 200 s，
  `render.frame_age_ms` 从 10.7 s 增长到 201.4 s；`dsp.frames` 同步冻结。
- CPU 0.1%（淡出瞬间 0.8–0.9%）；RSS 128,040 → 128,096 kB（+56 kB）。
- 重新播放测试音：下一次诊断行即 `Active`，`frame_age_ms=1`。

### Bars / Curve 与 blur on/off

- Bars 与 Curve 使用同一 `SpectrumSnapshot`；两者均无 crash、无 overrun。
- blur on/off 无法对照：客户端无法提交 blur region（见上），因此不存在“blur on”状态。

## Failures and Fixes

### 1. 启动即致命协议错误（已修复）

- 触发：在 niri 中运行任何本轮之前的 release 构建。
- 行为：启动约 16 ms 后 `Gdk-Message: Error 71 (协议错误) dispatching to Wayland display`，exit 1。
- 根因：GTK 4.24 已为窗口 surface 创建 `ext_background_effect_surface_v1`，`BackgroundEffect::new`
  再次对同一 surface 创建，niri 依协议发送 `wl_display.error`。
- 修复：删除 `src/render/background_effect.rs` 及其调用点（详见 Visual and Background Effect）。
- 验证：启动成功；`niri msg layers` 仅一个 `fft-plug` surface；无协议错误；Bars/Curve 正常。

### 2. 半透明柱体几乎不可见（已修复）

- 触发：任何 Bars 画面。
- 行为：材质 alpha 0.45 × lifecycle 0.92 应得 ~0.41，实际写回 alpha 为 0.17（`glReadPixels` = 44/255），
  画面接近透明。
- 根因：fragment shader 输出 straight alpha，混合使用 `SRC_ALPHA/ONE_MINUS_SRC_ALPHA`，写入 RGBA
  framebuffer 后 alpha 被平方。
- 修复：shader 输出预乘 alpha `vec4(rgb*a, a)`，混合改为 `ONE/ONE_MINUS_SRC_ALPHA`；读回 alpha = 106/255 (0.41)。
- 验证：白噪声截图满宽 64 根柱子颜色正常；同区域颜色采样显示柱体覆盖率提高、壁纸透出减少。

## Remaining Limitations

- **逐柱毛玻璃（V3）受环境阻塞**：GTK 4.24 独占 surface 的 background-effect 对象且不暴露 region API。
  `niri/fft-plug.kdl` 仍然保留，但在当前 GTK 下不产生可见模糊；这是降级路径，不是崩溃。
- **多显示器**：当前环境只有一个输出，未验证多输出默认行为与 connector 无关性。
- **壁纸轮换 / 明暗壁纸对照**：未执行，避免修改用户桌面状态。
- **长时间测试时长**：按用户批准缩短为 10 min active + 5 min silent（计划要求 30 + 10）。
- **频段分布**：band 边界为 `position^1.35`，低频占用少量 band（440 Hz 在 band 3）。这是既有 DSP
  设计，本轮未修改；是否改为严格对数属于新的产品取舍。
- 桌面证据（截图、trace、性能日志）保存在本轮会话的 `/tmp`，未提交到仓库；可复现命令见各章节。

## Final Decision

`Partially accepted: audio/recovery, layer-shell, GL lifecycle, scaling, refresh-rate and long-run checks pass in the current environment; per-bar background blur (V3) is blocked by GTK 4.24 owning the surface background-effect object; multi-monitor and wallpaper-rotation checks were not run.`

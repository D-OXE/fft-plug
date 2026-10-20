# fft-plug MVP 收尾 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to implement this plan task-by-task. Pi 是本轮唯一写入方；不要再委派其他写入代理。步骤使用复选框（`- [ ]`）追踪。

**Goal:** 在不扩展产品范围的前提下，补齐 `fft-plug` 当前 MVP 的性能可观测性、真实桌面恢复验证、视觉/缩放验收、长时间运行证据和文档收口，使项目达到可独立审阅的“当前环境 MVP 验收完成”状态。

**Architecture:** 保持现有 `PipeWire callback → 有界 PCM FIFO → DSP worker → latest snapshot → GTK/OpenGL` 架构不变。先补足不进入实时回调的低频诊断数据，再用 release 构建执行真实 niri/PipeWire 验收；只有复现明确缺陷后才修改相应模块，禁止为未出现的问题重写音频、Wayland 或 renderer 架构。

**Tech Stack:** Rust stable、PipeWire Rust bindings、`realfft`/`rustfft`、GTK 4、`gtk4-layer-shell`、`GtkGLArea`、OpenGL/GLSL、`ext-background-effect-v1`、niri。

**Spec:** `AGENTS.md`、`README.md`、`PLAN.md`、`PROJECT_SUMMARY.md`、`修改计划.md`。

## Global Constraints

- 本轮只收尾现有 MVP；不加入 MPRIS、歌词、设置中心、配置文件、指定 sink、多显示器独立 surface、第二 renderer、Vulkan/wgpu、glow/bloom/particles。
- 不改变 `4096` FFT window、`512` hop、`64` bands、约 `20 Hz–20 kHz`、默认 `punchy = 15/120 ms`，除非真实测量证明现值造成验收失败并由用户确认取舍。
- PipeWire process callback 仍然只能做有界解码、混音、PCM 发布和原子计数；不得加入锁、日志、FFT、文件 I/O 或随输入增长的分配。
- DSP 纯逻辑不得读取系统时钟、GTK、Wayland 或环境状态；snapshot age 在 pipeline 发布层测量，不写入 `SpectrumSnapshot` 的信号语义。
- 不自动修改 `~/.config/niri`，不自动重启 PipeWire/niri；这些外部状态动作在执行前取得用户明确同意。
- 不以限制到 60/120 Hz 解决性能问题；active render 继续跟随 GTK frame clock，SLEEPING 必须停止持续 redraw。
- 不实施 transient accent、VBO 重构或 64/96/128 bands A/B，除非 Task 5 的测量给出明确必要性，并先向用户说明收益与代价。
- 当前 `.git` 目录不可用。不得自行初始化仓库、暂存、提交或覆盖 `fft-plug.tar.zst`；若执行时 Git 已恢复，仍须遵守用户对 Git 写操作的授权边界。
- 所有“通过”结论必须来自本轮实际命令或桌面证据；历史记录只能作为参考。

## Current Verified Baseline（2026-10-20，Asia/Shanghai）

- `rustc 1.99.1`、`cargo 1.99.1` 可用。
- `cargo fmt --check`：通过。
- `cargo check --all-targets --all-features`：通过。
- `cargo clippy --all-targets --all-features -- -D warnings`：通过。
- `cargo test --all-targets --all-features`：32/32 通过。
- `cargo build --release --all-features`：通过。
- 尚未在本轮验证：真实音频响应、默认输出切换、PipeWire 重启、设备休眠/恢复、GL context 重建、分数缩放、多显示器、逐柱毛玻璃、长时间 CPU/内存/帧龄。

## File Map

- Modify: `src/audio/mod.rs` — 暴露 session/reconnect 低频计数，不改变 callback 数据路径。
- Modify: `src/pipeline/mod.rs` — 在 latest snapshot 存储层记录发布时间并报告 snapshot age。
- Modify: `src/render/mod.rs` — 通过已有 `last_frame` 报告最后一帧年龄，不增加每帧分配。
- Modify: `src/shell/mod.rs` — 把新增统计加入每秒一次的 `--debug` 输出。
- Conditional modify: `src/audio/mod.rs`, `src/pipeline/mod.rs`, `src/shell/mod.rs`, `src/render/mod.rs`, `src/render/background_effect.rs` — 仅修复真实验收中已经复现的缺陷。
- Create: `docs/validation/MVP_ACCEPTANCE.md` — 记录环境、步骤、原始结果摘要、失败/限制和最终判定。
- Modify: `README.md`, `PLAN.md`, `PROJECT_SUMMARY.md` — 只同步已经实现且已经验证的事实。

## Review Focus

1. 默认输出切换、PipeWire 重启、暂时无设备后，程序应恢复或保持可重试状态，不 panic、不残留失效线程；由 Task 3 验证。
2. SLEEPING 淡出完成后 render frame 计数应稳定，新音频到达后应重新安装 tick 并恢复渲染；由 Task 3 和 Task 5 验证。
3. `GtkGLArea` unrealize/realize 后 GL 与 background-effect 资源应正确重建，协议缺失时仍显示频谱；由 Task 4 验证。
4. 整数/分数缩放与多显示器默认行为不得产生越界几何、彩虹接缝或整层模糊底板；由 Task 4 验证。
5. 新增统计不得侵入 PipeWire realtime callback，也不得在每帧制造无界分配或普通 mutex 竞争；由 Task 2 的测试、Clippy 和最终 diff 审查验证。

---

### Task 1: 固化当前无设备质量基线

**Files:**
- Create later: `docs/validation/MVP_ACCEPTANCE.md`
- No production source changes.

**Interfaces:**
- Consumes: 当前工作区源码与 `Cargo.lock`。
- Produces: 后续任务可以比较的命令、版本、测试数量和 release 二进制。

- [ ] **Step 1: 记录工具链与桌面依赖版本**

Run:

```bash
rustc --version
cargo --version
pkg-config --modversion gtk4 libpipewire-0.3
niri --version
```

Expected: Rust/Cargo 可运行；缺少某个桌面命令时记录真实缺项，不安装或升级系统包，先交给用户决定。

- [ ] **Step 2: 执行全部无设备检查**

Run:

```bash
cargo fmt --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo test --no-default-features
cargo build --release --all-features
```

Expected: 全部 exit 0；默认 feature 测试至少保持当前 32 个测试全部通过。

- [ ] **Step 3: 建立验收记录骨架**

Create `docs/validation/MVP_ACCEPTANCE.md` with sections:

```markdown
# fft-plug MVP Acceptance
## Environment
## Build and Unit Checks
## Audio and Recovery
## Layer Shell and OpenGL
## Visual and Background Effect
## Performance and Long Run
## Failures and Fixes
## Remaining Limitations
## Final Decision
```

只写实际结果；尚未执行的项目标记 `Not run`，不能预填 `Passed`。

---

### Task 2: 补齐验收所需低频诊断指标

**Files:**
- Modify: `src/audio/mod.rs:46-140,244-290`
- Modify: `src/pipeline/mod.rs:113-138,373-403,501-513`
- Modify: `src/render/mod.rs:35-76,148-238`
- Modify: `src/shell/mod.rs:227-267`
- Test: 各模块现有内联 `#[cfg(test)]` 测试区。

**Interfaces:**
- Consumes: `AudioStatus::snapshot()`, `Pipeline::stats()`, `GlRenderer::stats()`。
- Produces:
  - `AudioStats::session_generation: u64`
  - `AudioStats::reconnects: u64`
  - `PipelineStats::snapshot_age_ms: Option<u64>`
  - `RenderStats::frame_age_ms: Option<u64>`
  - 每秒 debug 行中的 `audio.sessions`、`audio.reconnects`、`dsp.snapshot_age_ms`、`render.frame_age_ms`

- [ ] **Step 1: 为 AudioStatus 写失败测试**

在 `src/audio/mod.rs` 增加：

```rust
#[test]
fn audio_status_counts_session_generations_and_reconnects()
```

断言：新状态为 `0/0`；第一次 `begin_session()` 后为 `1/0`；第二次后为 `2/1`。测试不得创建真实 PipeWire 对象。

- [ ] **Step 2: 运行 AudioStatus 定向测试，确认先失败**

Run:

```bash
cargo test --all-features audio::tests::audio_status_counts_session_generations_and_reconnects -- --exact
```

Expected: FAIL，因为 `begin_session` 和统计字段尚不存在。

- [ ] **Step 3: 实现 session/reconnect 统计**

在 `AudioStatus` 增加两个原子计数，并实现：

```rust
fn begin_session(&self)
```

第一次 session 不计为 reconnect；后续每次进入 `run_session` 前增加 reconnect。只在 PipeWire 管理线程调用，不在 process callback 中增加新工作。

- [ ] **Step 4: 运行 AudioStatus 定向测试，确认通过**

Run:

```bash
cargo test --all-features audio::tests::audio_status_counts_session_generations_and_reconnects -- --exact
```

Expected: PASS。

- [ ] **Step 5: 为 SnapshotStore 写失败测试**

在 `src/pipeline/mod.rs` 增加：

```rust
#[test]
fn snapshot_store_reports_none_before_publish_and_age_after_publish()
```

断言：发布前 `age()` 为 `None`；发布一个最小 `SpectrumSnapshot` 后，`latest()` 返回相同 `frame_index`，`age()` 为 `Some` 且不超过 1 秒。

- [ ] **Step 6: 运行 SnapshotStore 定向测试，确认先失败**

Run:

```bash
cargo test --no-default-features pipeline::tests::snapshot_store_reports_none_before_publish_and_age_after_publish -- --exact
```

Expected: FAIL，因为 `SnapshotStore::age()` 尚不存在。

- [ ] **Step 7: 在 snapshot 发布层记录年龄**

将 `SnapshotStore` 内部值改为私有 entry：

```rust
struct PublishedSnapshot {
    snapshot: Arc<SpectrumSnapshot>,
    published_at: Instant,
}

pub fn age(&self) -> Option<Duration>
```

`latest()` 的现有返回类型保持 `Option<Arc<SpectrumSnapshot>>`。`Pipeline::stats()` 将年龄转换为 `Option<u64>` 毫秒；不得把时钟放入 `dsp::SpectrumSnapshot`。

- [ ] **Step 8: 运行 SnapshotStore 定向测试，确认通过**

Run:

```bash
cargo test --no-default-features pipeline::tests::snapshot_store_reports_none_before_publish_and_age_after_publish -- --exact
```

Expected: PASS。

- [ ] **Step 9: 为 RenderStats 写失败测试**

在 `src/render/mod.rs` 增加：

```rust
#[test]
fn renderer_stats_reports_none_before_first_frame_and_age_after_frame()
```

测试直接设置测试模块可见的 `last_frame`，不创建 GTK/GL context；断言新 renderer 为 `None`，设置后为 `Some` 且不超过 1 秒。

- [ ] **Step 10: 运行 RenderStats 定向测试，确认先失败**

Run:

```bash
cargo test --all-features render::tests::renderer_stats_reports_none_before_first_frame_and_age_after_frame -- --exact
```

Expected: FAIL，因为 `RenderStats::frame_age_ms` 尚不存在。

- [ ] **Step 11: 实现 frame age 与 debug 输出**

`GlRenderer::stats()` 通过现有 `last_frame: Option<Instant>` 计算 `frame_age_ms`。在 `src/shell/mod.rs` 的低频 debug 行加入四个新字段；字段缺失时打印 `none`，禁止为格式化而在 realtime callback 中写日志。

- [ ] **Step 12: 验证 RenderStats 定向测试与全部新增诊断**

Run:

```bash
cargo fmt --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo test --no-default-features
```

Expected: 全部通过；无设备测试数量增加至少 3 个。

---

### Task 3: 完成真实 PipeWire 音频与恢复验收

**Files:**
- Conditional modify: `src/audio/mod.rs`
- Conditional modify: `src/pipeline/mod.rs`
- Conditional modify: `src/main.rs`
- Update evidence: `docs/validation/MVP_ACCEPTANCE.md`

**Interfaces:**
- Consumes: Task 2 新增的 session/reconnect、snapshot age 和现有 PCM/DSP/activity 统计。
- Produces: 当前环境中启动、静音/唤醒、默认输出切换、服务重启和无设备恢复的可核验结果；只在失败时产生最小代码修复。

- [ ] **Step 1: 从 release 二进制建立真实音频基线**

Run:

```bash
cargo build --release --all-features
./target/release/fft-plug --debug --mode bars --dynamics punchy
```

另一个终端使用 `wpctl status` 找到当前真实 sink，并向该 sink 播放已知测试音；不得复制历史硬编码 node id。

Expected: `audio.state=Connected`，sample rate/channels/format 非零，callback/PCM/DSP/render 计数增长，peak 非零，snapshot age 保持有界。

- [ ] **Step 2: 验证静音、睡眠和自动唤醒**

停止测试音，等待 `DECAYING → SLEEPING` 和最终淡出；连续观察至少 10 秒。再次播放测试音。

Expected: SLEEPING 后 render frame 计数停止增长、frame age 增长；恢复音频后约 250 ms 内 activity 回到 ACTIVE，render frame 重新增长，不重启进程。

- [ ] **Step 3: 验证默认输出切换**

在两个真实可用输出之间切换系统默认 sink，并分别播放测试音。若当前只有一个可用输出，记录硬件阻塞，不伪造切换结论。

Expected: stream 自动移动或进入受控重连，session/reconnect 统计能解释行为；新的输出有非零 peak，旧输出不再驱动画面；程序不 panic、不刷屏。

- [ ] **Step 4: 验证 PipeWire 重启和暂时无设备**

这是外部状态变化，执行前取得用户明确同意。分别验证 PipeWire 服务重启以及输出设备短暂不可用后恢复。

Expected: 程序进入 Starting/Error/Paused 等可解释状态，随后恢复 Connected；若平台不能透明恢复，必须给出低频、可操作的重试错误，不允许崩溃或每 callback 刷日志。

- [ ] **Step 5: 仅对已复现失败执行系统化修复**

如果任一步失败：使用 `debug` / `superpowers:systematic-debugging`，先保存复现日志、定位触发条件，再为可分离逻辑写失败测试。除非证据表明 PipeWire `AUTOCONNECT + STREAM_CAPTURE_SINK` 无法跟随默认 sink，否则不要提前引入 metadata registry、设备选择 CLI 或新的公共抽象。

- [ ] **Step 6: 回归验证并记录**

重复失败场景、正常启动、静音唤醒和全部无设备测试。把实际 node、格式、状态转换、恢复时间和任何限制写入验收记录。

---

### Task 4: 完成 layer-shell、GL 生命周期、缩放与毛玻璃验收

**Files:**
- Conditional modify: `src/shell/mod.rs`
- Conditional modify: `src/render/mod.rs`
- Conditional modify: `src/render/background_effect.rs`
- Conditional modify: `src/render/color.rs`
- Conditional modify: `niri/fft-plug.kdl`
- Update evidence: `docs/validation/MVP_ACCEPTANCE.md`

**Interfaces:**
- Consumes: 当前 `GlRenderer::{realize,render,unrealize}`、`BackgroundEffect::{new,update,clear}` 和 `niri/fft-plug.kdl`。
- Produces: Bars/Curve、逐柱 blur、降级路径、context 重建、缩放和多显示器行为的桌面证据。

- [ ] **Step 1: 验证 layer-shell 契约**

使用 niri 的公开层级查询确认 namespace=`fft-plug`、layer=`bottom`、keyboard interactivity=`none`、exclusive zone=`0`、bottom+left+right anchors 和 320 logical px 高度。

Expected: 普通窗口可覆盖频谱，应用不抢焦点，surface 宽度由 compositor 协商。

- [ ] **Step 2: 验证 Bars 与 Curve 基本回归**

分别运行 `--mode bars` 和 `--mode curve`，使用同一测试音和 dynamics profile。

Expected: 两种模式的峰值频率/响度语义一致；Curve 不启用逐柱 blur region；无 NaN、闪黑、残留顶点或 shader 错误。

- [ ] **Step 3: 验证逐柱毛玻璃与正式降级路径**

经用户同意后手工 include/copy `niri/fft-plug.kdl`；分别在启用和禁用该 rule 的环境运行。检查明亮、暗色、高对比和高饱和壁纸，并触发一次壁纸轮换。

Expected: 启用时只有柱体覆盖区域模糊，gap 和无柱区域清晰；壁纸更新不要求重启；禁用 rule 或缺少协议时仍显示透明彩虹柱并正常退出。

- [ ] **Step 4: 验证刷新率与缩放**

在实际可用的 60/120/160 Hz 中至少测试两档；测试整数缩放和一种分数缩放。若硬件只提供一种刷新率或缩放组合，记录外部限制，不把该项标为通过。

Expected: 30 秒彩虹周期不随 FPS 明显改变，柱体无接缝，高光不偏移，逻辑尺寸/scale debug 值与 niri 输出一致。

- [ ] **Step 5: 验证多显示器和 GL context 重建**

在硬件可用时至少连接/启用两个输出，记录默认 surface 所在输出及行为；通过当前桌面可用的输出禁用/启用或 surface 重建路径触发一次 `GtkGLArea` unrealize/realize。硬件不足时记录阻塞，不声称多显示器已验证。

Expected: 不硬编码 connector，不崩溃；GL program/VAO/VBO 和 background effect 被重新创建，恢复后画面继续更新且无旧 blur region 残留。

- [ ] **Step 6: 仅修复已复现的视觉或生命周期缺陷**

几何/颜色错误先在现有纯逻辑 helper 上加失败测试；GL/Wayland 生命周期错误保留最小手工复现记录。不得因单一 compositor 差异增加第二 renderer 或读取壁纸像素。

---

### Task 5: 完成长时间性能与资源验收

**Files:**
- Conditional modify: 已测量到瓶颈的单一模块。
- Update evidence: `docs/validation/MVP_ACCEPTANCE.md`

**Interfaces:**
- Consumes: Task 2 debug 指标和 release 二进制。
- Produces: active、decaying、sleeping 三种状态的 CPU、RSS、callback、FIFO、snapshot/frame age 和 render cadence 数据。

- [ ] **Step 1: 定义并记录测量环境**

记录 CPU、GPU、显示分辨率/刷新率/缩放、PipeWire 格式、Bars/Curve、壁纸效果是否启用。性能数据只使用 release 构建。

- [ ] **Step 2: 执行 30 分钟 active 混合负载**

至少包含低频鼓点、宽频音乐、短暂停顿和音量变化；每 5 分钟记录 CPU、RSS、callbacks、PCM queued/overruns、DSP frames、snapshot age、render frames/frame age。

Expected: 无 crash；无持续 PCM overrun；DSP 在 48 kHz/512 hop 下接近约 94 Hz；render 活动时跟随实际 frame clock；RSS 在预热后不呈持续单调增长。

- [ ] **Step 3: 执行 10 分钟静音负载并唤醒**

Expected: fade 后 render frames 保持稳定，DSP worker 不高频轮询，CPU 接近空闲；唤醒后指标恢复且不出现一段旧 PCM 的延迟回放。

- [ ] **Step 4: 比较 Bars/Curve 与 blur on/off**

使用同一音频和显示环境记录四组数据。这里的目标是发现数量级回归，不为微小差异提前重构。

- [ ] **Step 5: 只优化已定位的瓶颈**

若存在明确性能问题，使用 `optimize` 技能先建立前后测量。禁止用无界缓存、降低 DSP 正确性或固定 60/120 FPS 掩盖问题；VBO 预分配只有在 `glBufferData`/分配被测量为热点时才进入实现。

- [ ] **Step 6: 重跑全量回归**

任何性能修改后重复 Task 1 全部命令，以及 Task 3 的音频恢复和 Task 4 的 GL/sleep 关键路径。

---

### Task 6: 文档收口与交给 Codex 审阅

**Files:**
- Modify: `README.md`
- Modify: `PLAN.md`
- Modify: `PROJECT_SUMMARY.md`
- Finalize: `docs/validation/MVP_ACCEPTANCE.md`

**Interfaces:**
- Consumes: Tasks 1–5 的实际代码、命令输出和桌面证据。
- Produces: 不夸大状态的项目文档，以及 Codex 可以独立核验的审阅对象。

- [ ] **Step 1: 修正状态文档中的已知矛盾**

`PLAN.md` 顶部当前仍写“第一版只使用整个 surface 毛玻璃、不实现逐柱 region”，而后文和源码已实现逐柱 `ext-background-effect-v1` region。改成“代码路径已实现；是否完成 V3/V4 取决于本轮真实桌面验收结果”。

- [ ] **Step 2: 只按证据更新阶段状态**

只有对应验收全部通过时，才把阶段 7/8、V3/V4 标为完成。失败、未运行或只有历史记录的项目必须保留为“未验证”或“受环境阻塞”。

- [ ] **Step 3: 完成验收记录**

每个章节写明日期、环境、命令/动作、结果、关键计数和限制；避免粘贴海量日志，只保留可复现命令和关键原始行。`Final Decision` 只能是以下之一：

- `Accepted: current-environment MVP complete`
- `Partially accepted: <明确未通过项>`
- `Blocked: <外部环境或权限原因>`

- [ ] **Step 4: 执行最终质量门**

Run:

```bash
cargo fmt --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo test --no-default-features
cargo build --release --all-features
```

Expected: 全部 exit 0。失败必须保留并分类为本次回归、原有问题或环境问题，不能通过弱化测试制造通过。

- [ ] **Step 5: 自审所有实际文件**

若 Git 可用，审查 staged、unstaged 和新增文件的完整 diff；若仍不可用，逐个列出本轮修改/新增文件并直接复读完整内容，重点检查 realtime callback、unsafe/FFI、线程关闭、GL 生命周期、CLI/公开统计和文档状态。

- [ ] **Step 6: 准备 Codex 审阅交接**

向用户提供：任务目标、实际修改文件、每项验证结果、未验证项、新增文件、0–3 个高风险审阅点。若 `git rev-parse --git-path AI_HANDOFF.md` 可解析且可写，按 `AGENTS.md` 生成交接文件；否则在回复中提供等价交接，不因此初始化 Git。

Codex 后续审阅范围：实际源码与文档、Pi 的验证证据、当前环境复现、接口兼容性、realtime/unsafe/线程/GL 生命周期风险。Pi 的“完成”声明不等于 Codex 独立审阅通过，也不等于用户验收。

## Explicitly Deferred After This Plan

- transient / spectral-flux accent；
- 96/128 bands 或动态 band density；
- Oklch/Oklab 色环；
- glow、bloom、particles、radial、waveform；
- 指定 PipeWire sink、多显示器独立 surface；
- 配置文件、设置 GUI、systemd user service、发行版打包；
- MPRIS、歌词、封面或任何网络功能。

这些项目只有在当前 MVP 完成、用户提出新目标并有单独规格后再进入计划。

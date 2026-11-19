# 查看端性能分析 / Viewer performance analysis

Abstract (English). Measurements of the SDL viewer's receive → decode → present path on a
4-core Skylake machine (X11, Intel iGPU, no HEVC hardware decode). The network is not the
bottleneck: the previous path sustained ~11-12 fps at a 4K remote desktop because software
decoding (~47 ms/frame) and a freshly created RGB24 texture upload (~36-41 ms/frame) ran
serially on the event-loop task, and the same task blocked 75-144 ms per write into the
ffmpeg decoder process. Access-unit queue overflow then reset the decoder and requested key
frames, which turned the shortfall into visible stalls.
The P0 items below are implemented: the same 4K H.265 stream now decodes in ~29 ms/frame and
uploads in 5 ms, i.e. ~29 fps instead of ~11, and neither decode nor encode writes block the
event loop. Sections 4-5 keep the remaining backlog with its measured gain and the reasons
items were skipped.

本文记录实测、已实现的改动和剩余待办。数据来自本机实测（4 核 / Skylake iGPU / X11，
`vainfo` 显示只有 MPEG2/H264/VP8 的 VLD，无 HEVC 解码），复现命令见第 7 节。

## 1. 数据通路

改动前：

```
rtc.rs on_track ─ track.read_rtp ─ media::Receiver (40 ms gap wait, NACK/PLI)
      └─ desktop.units: Latest<AccessUnit>(8)
            └─ desktop::pump   8 ms tick, 4 ms 预算, 最多 8 个 AU, 只显示最新帧
                  ├─ decoder_cli::decode  → ffmpeg 子进程 (threads 1, rgb24)
                  │     sync_channel(2) 帧队列, 满即丢
                  └─ sdl::draw           每帧新建纹理 → update → copy → present
```

改动后：

```
rtc.rs on_track ─ media::Receiver ─ desktop.units (8) ─┐ 溢出只置 thin（丢到下一个关键帧）
desktop::pump ─ decoder_cli::decode ─ 写线程 sync_channel(2) → ffmpeg (threads 0, nv12)
                                    └ Arc<Frame> ─ sdl::draw 复用 NV12 纹理 → copy → present
```

关键点没有变：`pump` 和 `draw` 仍在 `select!` 的 8 ms tick 分支里同步执行，与输入、音频泵、
UI 事件共用一个任务。所以每帧的 CPU 时间要短，任何一次写管道都不能阻塞（现在已移走）。

## 2. 实测数据

### 2.1 解码 + 管道（`src/viewer/decoder_cli.rs`）

| 场景 | 改动前（`-threads 1` → rgb24） | `-threads 0` + rgb24 | `-threads 0` + nv12 |
|---|---|---|---|
| 1080p60 H.264（360 帧） | 130 fps | 153 fps | **184 fps** |
| 1080p60 H.265（240 帧） | 97 fps | — | **125 fps** |
| 4K30 H.264（90 帧） | 49 fps | 48 fps | **61 fps** |
| 4K30 H.265（120 帧） | **21 fps** | 28 fps | **37 fps** |

`thread_type slice` 与 `threads 2/3` 都更慢（4K HEVC 3.1-4.1 s vs `threads 0` 2.9 s），
所以保持 ffmpeg 默认的 frame threading。

### 2.2 SDL 纹理上传 + 绘制（`src/viewer/sdl.rs`）

| 方式 | 1080p | 4K |
|---|---|---|
| 每帧 `create_texture_streaming` + update + copy + present（改动前） | 10.7 ms | **36-41 ms** |
| 复用纹理 + RGB24 update | 8.4 ms | 24.9 ms |
| 复用纹理 + **NV12** update | **3.2 ms** | **5.8 ms** |

本机 GL 渲染器在 `SDL_RendererInfo.texture_formats` 里广告 `SDL_PIXELFORMAT_NV12/IYUV/YV12`，
`SDL_UpdateTexture` 对 NV12 纹理返回 0，渲染结果取回像素验证过色度正确（红/蓝各测一次），
也就是 GPU 做 YUV→RGB，CPU 不 swscale。另测 BGRA8888 上传并不比 RGB24 快
（4K 27.3 vs 24.9 ms），所以收益来自位深减半而非格式对齐。

### 2.3 写入 ffmpeg stdin 的阻塞（改动前在事件循环内）

向改动前参数的 ffmpeg 灌 4K 码流（读端持续消费）：

| 码流 | 最长单次 `write_all` | >5 ms 的写入 |
|---|---|---|
| 4K H.265 | **144 ms** | 83 / 207 |
| 4K H.264 | **75 ms** | 88 / 704 |

`-probesize 32 -analyzeduration 0` 只影响解封装，ffmpeg 仍按解码速度消费输入，所以管道一满
写入就阻塞。这期间 tick 循环、输入转发、音频泵全部停摆。

### 2.4 硬件解码在本机不划算

`VAProfileH264High: VAEntrypointVLD` 有，HEVC 没有。实测 H.264 VAAPI（`hwdownload,format=nv12`）
2.98 s/360 帧（≈121 fps），比软件 `-threads 0 nv12`（1.96 s）还慢：每帧一次 `hwdownload` 同步
抵消了收益。硬件解码只有在能把帧留在 GPU 上（不走管道）时才有意义。

### 2.5 每帧分配

4K NV12 每帧 12.4 MB。`vec![0; size]` 走 mmap，清零由内核惰性提供；实测
"malloc + memset + free" 与 "malloc + 触页 + free" 都是 0.0-0.2 ms/帧，与 29 ms 解码相比可忽略，
因此没有做缓冲池（见 4 节第 5 条）。

## 3. 4K 场景的预算：改动前 / 后

```
改动前                                    改动后
解码           47 ms/帧                  ~29 ms/帧（threads 0 + nv12）
纹理上传+绘制  36-41 ms/帧                ~5 ms/帧（复用 NV12 纹理）
──────────────────────────                ──────────────────────────
合计          ~85 ms/帧 → 11-12 fps       ~34 ms/帧 → ~29 fps
写管道         75-144 ms 阻塞事件循环     0（独立写线程，队列满即丢）
```

同一套读帧计时脚本在本机复测 4K H.265：改动前 18.3 fps（rgb24 + threads 1），改动后
34.4 fps（nv12 + threads 0）；1080p H.264 109 → 134 fps。合上 2.2 的上传时间，4K 端到端
约 10.7 → 29 fps，1080p 约 50 → 94 fps。

主机默认 H.265（硬件编码且双方都支持时选 HEVC），所以 4K 下默认仍是最慢的一条路；本机无
HEVC 硬解时可以用 `UU_VIEWER_CODEC=h264` 换 61 fps 的软解（代价是同样码率下画质略降）。

溢出策略也随之改了：AU 队列只有 8 帧（≈133 ms），解码慢于码流时必然溢出。原来的做法是
`reset + pli`（flush 解码器、等关键帧、整批 P 帧丢弃），每次溢出都要等最多一个 GOP；现在
只置 `thin`：丢掉间隙之后的非关键帧，解码器原样保留，到下一个 IDR 立刻恢复，同时照常发一次
250 ms 限流的 PLI 催主机早点发 IDR。ffmpeg 的写队列满（第 4 条）走同一套逻辑，只是把 `thin`
留在 `Cli` 内部。效果是：解码器始终按本机上限满速跑，卡顿从“重启+等 GOP”变成“跳过一两个
间隔”，画面不再有额外的空白期。

## 4. 清单与状态

### P0（已实现）

1. **复用纹理** — `src/viewer/sdl.rs` 的 `upload()` / `blit()`
   `Window` 里缓存 `texture`（视频）与 `hud`（图层）两张纹理，只在尺寸/像素格式变化时重建。
   4K 36-41 → 5.8 ms（配合 NV12），1080p 10.7 → 3.2 ms。纹理以 `Texture<'static>` 保存，
   `Window` 先声明它们再声明 `canvas`，drop 顺序保证纹理先于渲染器销毁（`unsafe` 只有这一处，
   同一结构体字段顺序保证）。

2. **解码输出与纹理都用 NV12** — `src/viewer/decoder_cli.rs`（`-pix_fmt nv12`）、
   `src/viewer/sdl.rs`（`PixelFormatEnum::NV12`）。`Frame` 现在带 `pixel: Pixel::{Rgb24,Nv12}`
   与 `to_rgb()`；只有低频路径（`thumbs::save`、`type_ipc::write_png_rgb`、
   `UU_VIEWER_DUMP_FRAME`）按需做一次 BT.709 limited-range YUV→RGB。
   管道字节数减半（4K 每帧 12.4 MB vs 24.9 MB），解码 47 → 29 ms。

3. **`-threads 1` → `-threads 0`** — `src/viewer/decoder_cli.rs`
   可用 `UU_VIEWER_DECODE_THREADS=1` 换最低延迟，默认 0（每核一线程的 frame threading）。

4. **把写 ffmpeg 的活儿移出事件循环** — `src/viewer/decoder_cli.rs`
   `Cli` 内部起写线程，`decode()` 只把 AU 投进 `sync_channel(2)`，事件循环不再有 75-144 ms 的停顿。
   队列满 = 解码器落后于码流：丢掉这个 AU，并跳过后续非关键帧直到下一个关键帧（解码器不重启），
   同时通过新加的 `Decoder::needs_key_frame` 让上层发一次 PLI，让主机尽快给 IDR。

### P1

5. **去掉每帧两次整帧拷贝 — 已实现一半**
   `sdl::present` 不再 `rgb.clone()`，改为把 `Arc<Frame>` 存进 `last_frame`（原来 4K 每帧复制
   25 MB）。读帧缓冲的 `vec![0; size]` 保留：实测每帧分配/清零 <0.2 ms（见 2.5），
   做缓冲池要引入归还通道和 `Frame` 的 `Drop`，不值得。

6. **溢出不再触发 reset + PLI 风暴 — 已实现**
   `src/viewer/rtc.rs` 溢出置 `desktop.thin`，`src/viewer/desktop.rs` 的 pump 看到后只 `gate.arm()`
   等下一个关键帧，不 `decoder.reset()`。测试
   `actual_queue_loss_thins_to_a_key_frame_without_restarting_the_decoder` 覆盖这条路。

7. **非全屏窗口默认“跟随当前窗口” — 未采用**
   `src/viewer/hud.rs` 仍是 `follow_remote: true`。原因是这条会让被控端按查看窗口的像素尺寸
   走分辨率请求，X11 主机在 `UU_DESKTOP_RESIZE` 默认值下可能直接切换显示模式（见 AGENTS.md），
   对只是想看大屏的用户是破坏性行为。保留代码路径，由用户在控制台里主动选。

### P2（画质，未改动）

8. **默认“自动”= CQ 18 + 8 Mbit/s 上限** — `src/capture_setting.rs`、`src/video_governor.rs`
   局域网下“原画 20M”明显更清楚。要改就得让自动档按链路健康度（RTT/丢包）抬高上限，
   否则弱网会被打爆；属于策略改动，留待单独评估。

9. **真彩 4:4:4** — `src/viewer/hud.rs`，默认 `chroma: 1`。4:4:4 是 1.5 倍码率，
   码率不足时反而更糊；先解决带宽策略（第 8 条）再谈默认值。

10. **无 HEVC 硬解时优先 H.264** — `src/viewer/wire.rs` 的 `video_preferences(h264_only)`。
    查看端现在永远是软解（ffmpeg CLI），所以硬件解码能力与选择无关；而 H.264 在 4K 下软解
    61 fps vs H.265 37 fps，但同码率画质更差。保持默认发两种编码由主机决定（主机硬件编码时选
    H.265），需要时用 `UU_VIEWER_CODEC=h264`。

### 音频（未改动）

11. `src/viewer/audio.rs`：SDL 队列目标 100 ms（`SDL_BYTES`）+ PCM `Latest(12)`×40 ms，
    且没有音画同步，声音比画面晚 100-200 ms。建议队列压到 ~60 ms、PCM 队列收到 3-4 个包，
    并在视频侧做一次简单的时间戳对齐。
12. 音频解码同样经过 ffmpeg 子进程（Ogg/Opus，额外 ~40 ms 与 4 ms 轮询）；webrtc-rs 自带
    Opus 解码，可改为进程内解码，去掉一个进程。

### 不要做的

- 本机开 VAAPI 硬解（无 HEVC VLD，H.264 也比软解慢，见 2.4）。
- 打开 `present_vsync`：`src/viewer/sdl.rs` 的 `into_canvas().build()` 默认关闭，
  保持关闭；打开会把 present 变成 16 ms 阻塞。

## 5. 目前的临时规避

| 手段 | 作用 |
|---|---|
| `UU_VIEWER_CODEC=h264` | 绕开 4K H.265 软解（37 fps → 61 fps） |
| `UU_VIEWER_DECODE_THREADS=1` | 换最低延迟（吞吐降到单线程水平） |
| 控制台 → 分辨率 →“跟随当前窗口”，窗口调小 | 主机编码端缩放，解码/上传/带宽等比下降 |
| 控制台 → 画质 →「原画 20M」/「超清 8M」 | 画质（默认自动档 CQ18 + 8 M 上限） |
| 控制台 → 色彩 →「真彩 4:4:4」 | 文字清晰度 |

## 6. 验证

```sh
cargo test --locked --features native-viewer,viewer-sdl --bin uuremote-viewer
cargo build --locked --features native-viewer,viewer-sdl --bin uuremote-viewer
```

单元测试覆盖：`decoder_cli` 的四种码流（H.264/H.265 × 4:2:0/4:4:4，原生与固定画布）都断言
NV12 尺寸与颜色；`desktop` 的 NV12→RGB 换算、`Arc<Frame>` 与溢出 thinning；`sdl` 的可读性测试
会分别用 NV12 与 RGB24 各画一帧，走真实的纹理缓存路径。

实机验收：1080p60 与 4K 各连一次，看 `Viewer desktop stats` 的 `presented` 是否跟得上
主机帧率、`decode_errors` 是否不再随溢出增长，并确认 `UU_VIEWER_DUMP_FRAME`、
缩略图（设备卡片预览）和 `type` 快照仍然正确（NV12 改动影响这三处 RGB 依赖）。

## 7. 复现测量

```sh
# 测试码流（本机生成，不含任何被控端画面）
ffmpeg -f lavfi -i testsrc2=s=1920x1080:r=60:d=6 -c:v libx264 -preset ultrafast -g 60 -pix_fmt yuv420p h264-1080.h264
ffmpeg -f lavfi -i testsrc2=s=3840x2160:r=30:d=4 -c:v libx265 -preset ultrafast -x265-params log-level=error:keyint=30 -pix_fmt yuv420p hevc-4k.hevc

# 2.1：解码 + 管道（读端持续消费）
ffmpeg -hide_banner -loglevel error -nostats -nostdin -threads 1 -f hevc -i hevc-4k.hevc -an \
  -pix_fmt rgb24 -fps_mode passthrough -f rawvideo pipe:1 | dd of=/dev/null bs=1M status=none

# 2.3：写入阻塞。按 32 KB 分块灌入，读端用线程持续消费，统计最长一次 write。
python3 - hevc-4k.hevc hevc <<'PY'
import subprocess, threading, time, sys
src, demux = sys.argv[1], sys.argv[2]
p = subprocess.Popen(["ffmpeg","-hide_banner","-loglevel","error","-nostats","-nostdin","-threads","1",
    "-probesize","32","-analyzeduration","0","-fflags","nobuffer","-flags","low_delay",
    "-f",demux,"-i","pipe:0","-an","-pix_fmt","rgb24","-fps_mode","passthrough","-f","rawvideo","pipe:1"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
frames = [0]
def reader():
    while p.stdout.read(3840*2160*3):
        frames[0] += 1
threading.Thread(target=reader, daemon=True).start()
data, start, worst, blocked = open(src,'rb').read(), time.time(), 0.0, 0
for i in range(0, len(data), 32768):
    t = time.time(); p.stdin.write(data[i:i+32768]); p.stdin.flush(); d = time.time()-t
    worst = max(worst, d); blocked += d > 0.005
p.stdin.close()
print(f"frames={frames[0]} total={time.time()-start:.2f}s worst_write={worst*1000:.0f}ms writes>5ms={blocked}")
PY
```

2.2 的纹理数据由一个最小 SDL2 程序拍出：同一张 `SDL_TEXTUREACCESS_STREAMING` 纹理分别以
“每帧新建 / 复用 RGB24 / 复用 NV12”三种方式 update + copy + present，统计 `ms/帧`。

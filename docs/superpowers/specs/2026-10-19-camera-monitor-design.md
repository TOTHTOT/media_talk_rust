# 监控页网络摄像头管理 — 设计文档

日期: 2026-10-19
状态: 已确认 (用户审批通过)

## 背景与目标

call-ui GUI 目前只有 SIP 对讲。web 端 (web-display) 已有"搜索摄像头 +
RTSP 拉流查看"能力, 本设计把同等能力搬进 GUI, 职责拆成两块:

- **设置页**: 摄像头的增删 (ONVIF 搜索添加 / 手动 RTSP 添加 / 已存列表删除)
- **监控页**: 只看 — 可折叠侧边栏切换已保存摄像头, 另一侧显示视频画面
  + 录制/截图按钮 (本期 toast"暂不支持"占位)

核心原则: **监控与对讲合并视频显示通路**。ingest (RTSP 拉流) 与 rtp_recv
(对讲收流) 的 tap 契约本来就是同一个类型 `VideoFrameSink` (RGBA
RawVideoFrame 回调, `ipcam-gst/src/ingest/tap.rs`), GUI 的
`make_frame_sink` (`call-ui/src/video.rs:9`) 对两者通用, 监控页直接复用
同一个 `video-frame` 属性, 只是数据源从 rtp_recv 换成 ingest。

## 方案选型

- **A (采纳)**: 复用 ingest, 补"本地查看"模式 — web 端和 GUI 共用同一条
  RTSP 拉流管线, 以后修 RTSP 的坑只修一处
- B (否决): 监控页独立写一条简单 RTSP 管线 — 重连/防卡/tap 契约重造,
  两套拉流并存
- C (否决): GUI 内走 WebRTC — 本机绕网纯浪费

## 架构与数据流

```
设置页: ONVIF 搜索 / 手动输入 ──► cameras 表 (rusqlite, Store 同库加表)
                                       │
监控页: 可折叠侧边栏 (已保存列表) ◄─────┘
           │ 选中
           ▼
        MonitorController (call-ui 新模块)
           │ ingest 本地模式 (rtspsrc 拉流, 音频关, 无 webrtcsink 支路)
           ▼
        VideoFrameSink (= 对讲同一个 make_frame_sink, 同一 video-frame 属性)
           ▼
        monitor_page.slint 视频区
```

## 模块改动

### 1. ipcam-gst/ingest — 本地查看模式 + tap 限宽

- `GstStreamConfig` 加"本地查看"开关: 不开 webrtcsink 转推支路, 音频
  `AudioOutput::Disabled`, 管线退化为 rtspsrc → decode → tap
- `tap_max_width: Option<u32>` 移植到 ingest tap 分支 (照 rtp_recv
  commit `c764e33` 的做法: videoscale + capsfilter PAR=1/1 + 限宽 +
  appsink max-buffers=1 drop=true)。1080p 摄像头不缩的话 ARM 板要付
  ~165MB/s RGBA 带宽

### 2. ipcam-discovery — 不动, call-ui 新增依赖

- 搜索: `probe_all_with_config(DiscoveryConfig)` (异步, ONVIF WS-Discovery)
- 选中设备后: `DeviceManagementClient::list_profiles()` +
  `get_stream_uri(token)` 取 RTSP URI

### 3. call-ui/store.rs — cameras 表

- 照 contacts/call_records 模式同库加表:
  `cameras(id, name, uri, username, password, created_at)`
- CRUD: add / list / delete
- 密码明文存本机 sqlite (门口机类设备惯例), 风险在设计文档记一笔,
  不做加密 (YAGNI)

### 4. call-ui/monitor.rs (新) — MonitorController

- 当前 ingest 句柄管理: 选中摄像头 = stop 旧 → start 新
- 启动失败 (RTSP 不通/认证错) → toast 提示, 视频区不占
- 运行中断流交给 ingest 现有 ReconnectPolicy, 不做额外 UI 状态机

### 5. 设置页 UI — 摄像头管理区块

- 已保存摄像头列表 (名称 + 地址, 可删除)
- "搜索添加": 扫描进度 → 设备列表 → 选 profile → 输账号密码 → 存库;
  超时/无结果在对话框内提示
- "手动添加": 名称 + RTSP URL + 账号密码 → 存库

### 6. 监控页 UI — monitor_page.slint (新)

- 可折叠侧边栏: 已保存摄像头列表 (只读, 无添加按钮 — 管理全在设置页);
  列表为空时提示"到设置页添加摄像头"
- 视频区 + 录制/截图按钮 (toast"暂不支持")
- `home/app_grid.slint:18` "监视"图标从 toast"开发中"改为 open-page(3)
- `persist.rs` 照 `wire_contacts` 模式接 `wire_cameras`

## 与对讲的冲突处理

- 监控页仅空闲 (phase=0) 可进
- 拨打/来电时 MonitorController 停掉 ingest 释放解码资源
- 通话结束**不自动恢复**, 用户重新点 (简单可靠, 避免状态纠缠)

## 预留不做 (记入 docs/pitfalls.md 待办)

- 侧边栏缩略图预览 (每路一个 VideoFrameSink 属性的接口形态, 用户明确
  说"预留接口并记录")
- 录制 / 截图 (按钮 toast 占位)
- 摄像头音频
- 多路宫格 (依赖 MPP 硬解, 见零拷贝路线待办)

## 错误处理

| 场景 | 行为 |
|---|---|
| RTSP 启动失败 (不通/认证错) | toast 提示, 视频区保持空 |
| 运行中断流 | ingest ReconnectPolicy 自动重连, 无额外 UI 状态 |
| 搜索超时/无结果 | 添加对话框内提示 |
| 摄像头列表为空 | 监控页侧边栏提示去设置页添加 |

## 测试

- store cameras CRUD 单测 (照 contacts 模式)
- ingest 本地模式 + tap 缩放管线测试: 能起 gst-rtsp-server 环回就自动化,
  依赖太重则真机手测并在 pitfalls 记录
- MonitorController 状态机单测 (选中/切换/停止, 不依赖 UI)

## 风险

- 摄像头密码明文落本机 sqlite — 门口机类设备惯例, 接受; 若日后有合规
  要求再加密
- ingest"本地查看"模式是 tap 支路无 webrtcsink 消费者的退化形态,
  管线改造时需保证 web 端现有转推路径不受影响 (回归: web 端播放)

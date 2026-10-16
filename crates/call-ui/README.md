# call-ui 模块详细机制

## 1. 线程模型

```mermaid
flowchart TB
    subgraph main["主线程: slint 事件循环"]
        UI[Slint UI]
        Props[刷属性]
    end

    subgraph worker["SIP Worker 线程 (独立 tokio runtime)"]
        Run["run() 主循环"]
        Cmd[handle_cmd]
        Incoming[handle_incoming]
        Outgoing[handle_outgoing]
        Media[media 模块]
    end

    Cmd --> Run
    Run --> Props
    Run --> Incoming
    Run --> Outgoing
    Incoming --> Media
    Outgoing --> Media
    UI --> Cmd
```

- **Slint UI** 在**主线程**跑事件循环，被 `#[tokio::main]` 阻塞
- **SIP worker** 在**独立线程**跑自己的 tokio runtime，不绑主线程
- 两边通过 `UiHandle::post`（worker→UI）和 `cmd_tx.send`（UI→worker）通信

---

## 2. 状态机

```mermaid
stateDiagram-v2
    [*] --> Idle: 注册成功 / 通话结束
    Idle --> Outgoing: dial()
    Idle --> Incoming: post(Incoming)
    Outgoing --> InCall: Confirmed
    Incoming --> InCall: Confirmed
    InCall --> Idle: Terminated / Failed
```

- `Idle`：初始状态，注册成功或通话结束后在此
- `Outgoing`：主叫拨号等待对方接听
- `Incoming`：被叫收到来电，等待用户操作
- `InCall`：通话中

---

## 3. 通道设计

### 3.1 UI → worker（tokio unbounded channel）

```mermaid
sequenceDiagram
    participant UI as Slint 回调<br/>(主线程)
    participant Ch as cmd_tx
    participant W as SIP worker<br/>(独立线程)

    UI->>+Ch: cmd_tx.send(WorkerCmd::Dial)
    Ch->>+W: handle_cmd(Dial)
```

- unbounded channel：Slint 回调是同步上下文，不能阻塞等消息

### 3.2 worker → UI（slint event loop）

```mermaid
sequenceDiagram
    participant W as SIP worker<br/>(任意线程)
    participant Post as UiHandle::post()
    participant SL as slint 事件循环<br/>(主线程)

    W->>Post: post(UiEvent::Incoming)
    Post->>SL: invoke_from_event_loop()
    SL->>SL: AppState.on_event()
    SL->>SL: 刷 slint 属性
```

- `post()` 把事件塞进 slint 事件循环，保证线程安全
- `slint::invoke_from_event_loop` 让任意线程可以操作 UI 状态

### 3.3 CallCtlTx / call_answer_ctl_rx（通话控制通道）

```
worker handle_cmd
    │
    ├─ prepare_dial() 创建 mpsc::channel(4) → (call_ctl_tx, call_answer_ctl_rx)
    │
    ├─ call_ctl_tx 存入 CallCtlTx 槽位 (Arc<Mutex<Option<Sender>>>)
    │     ↓
    │   UI 发 Hangup → handle_cmd → call_ctl_tx.lock() → try_send(Hangup)
    │
    └─ call_answer_ctl_rx 传给 OutgoingCtx / CallBase
          ↓
        outgoing_inner / wait_user_decision / call_loop 用它监听 Hangup
```

**类型定义** (`types.rs`):
```rust
// 控制通道发送端存入槽位 (Arc<Mutex<Option<Sender>>>)
pub type CallCtlTx = Arc<Mutex<Option<mpsc::Sender<WorkerCmd>>>>;

// 控制通道接收端传给通话任务
pub call_answer_ctl_rx: mpsc::Receiver<WorkerCmd>
```

**流程**:
1. **拨号时** — `prepare_dial` 创建 channel，`call_ctl_tx` 存入槽位
2. **挂断时** — `handle_cmd` 从槽位取出 `call_ctl_tx`，发送 `Hangup`
3. **通话任务监听** — `call_answer_ctl_rx.recv()` 收到后处理

### 3.4 CallRoutes / UpdateMsg（re-INVITE 路由通道）

```
forward_update (on_update 闭包)
    │
    ├─ routes: Arc<Mutex<HashMap<DialogId, mpsc::Sender<UpdateMsg>>>>
    │     ↓
    │   key = DialogId, value = update_tx (通话任务持有)
    │
    └─ UpdateMsg = (rsip::Request, oneshot::Sender<Option<Vec<u8>>>)
          ↓
        通话任务从 update_rx 收到后处理 re-INVITE
```

**类型定义** (`ipcam_sip::call_route`):
```rust
// re-INVITE 请求 + 回复通道
pub type UpdateMsg = (rsip::Request, oneshot::Sender<Option<Vec<u8>>>);

// 通话路由表
pub type CallRoutes = Arc<Mutex<HashMap<DialogId, mpsc::Sender<UpdateMsg>>>>;
```

**流程**:
1. **来电/拨号成功时** — `routes.lock().insert(dialog_id, update_tx)`
2. **re-INVITE 到来时** — `forward_update` 经 `routes` 找到 `update_tx`
3. **通话任务收到** — `update_rx.recv()` → 处理 → `oneshot::Sender` 回 answer SDP
4. **通话结束时** — `RouteGuard` drop 或手动 `routes.lock().remove(&id)` 摘除

### 3.5 通道总览图

```mermaid
flowchart LR
    subgraph worker["SIP worker 线程"]
        subgraph slot["CallCtlTx 槽位"]
            CTX[Arc&lt;Mutex&lt;Option&lt;ctl_tx&gt;&gt;&gt;]
        end
        subgraph routes["CallRoutes"]
            RT[HashMap&lt;DialogId, update_tx&gt;]
        end
    end

    UI -->|cmd_tx.send| handle_cmd
    handle_cmd -->|Dial| prepare_dial
    prepare_dial -->|call_ctl_tx| CTX
    prepare_dial -->|call_answer_ctl_rx| OutgoingCtx

    handle_cmd -->|Hangup| CTX
    CTX -->|call_ctl_tx.send| call_answer_ctl_rx

    OutgoingCtx -->|call_answer_ctl_rx| outgoing_inner
    OutgoingCtx -->|call_answer_ctl_rx| call_loop

    RIL -->|re-INVITE| forward_update
    forward_update -->|routes.get| RT
    RT -->|update_tx.send| update_rx
    update_rx -->|UpdateMsg| call_loop
```

图例:
- `CTX` = CallCtlTx 槽位
- `RT` = CallRoutes 路由表
- `call_ctl_tx` = 控制通道发送端（存入 CTX 槽位）
- `call_answer_ctl_rx` = 控制通道接收端（通话任务监听）
- `update_tx/rx` = re-INVITE UpdateMsg 通道

---

## 4. 被叫来电流程

```mermaid
sequenceDiagram
    participant PBX as rustpbx
    participant RIL as run_incoming_loop
    participant DL as dialog_layer
    participant RSL as run_dialog_state_loop
    participant MIH as make_incoming_handler<br/>(闭包)
    participant HI as handle_incoming
    participant UI as UI (post)
    participant Med as media 模块

    PBX->>+RIL: INVITE
    RIL->>+DL: get_or_create_server_invite(state_sender)
    DL-->>-RIL: dialog
    RIL->>DL: dialog.handle()

    RSL->>+MIH: DialogState::Calling(id)
    MIH->>+HI: spawn handle_incoming()

    HI->>+UI: post(Incoming { caller })
    HI->>DL: dialog.ringing()
    UI-->>-HI: 更新来电弹窗

    Note over HI: 等待用户点 Accept/Reject/超时/对端取消

    alt 用户点 Accept
        HI->>+Med: media::start_incoming()
        HI->>DL: dialog.accept(200 OK)
        HI->>+UI: post(Confirmed)
        Med-->>-HI: MediaHandles
        UI-->>-HI: 切换通话界面
    else 用户点 Reject
        HI->>DL: dialog.reject(Decline / 603)
        HI->>+UI: post(Terminated)
    else 来电超时 (30s)
        HI->>DL: dialog.reject(TemporarilyUnavailable / 480)
        HI->>+UI: post(Terminated)
    else 对端取消
        HI->>+UI: post(Terminated)
    end
```

---

## 5. 主叫拨号流程

```mermaid
sequenceDiagram
    participant UI as Slint 回调
    participant Cmd as handle_cmd
    participant HO as handle_outgoing
    participant DL as dialog_layer
    participant UI2 as UI (post)
    participant Med as media 模块

    UI->>+Cmd: cmd_tx.send(Dial)
    Cmd->>+HO: spawn handle_outgoing()

    HO->>+DL: do_invite(INVITE)
    DL-->>-HO: 200 OK
    HO->>HO: parse_answer_all()
    HO->>+UI2: post(Confirmed)
    HO->>+Med: media::start_outgoing()
    UI2-->>-HO: 切换通话界面
    Med-->>-HO: MediaHandles
```

---

## 6. re-INVITE 处理

```mermaid
sequenceDiagram
    participant RIL as run_incoming_loop
    participant DL as dialog_layer
    participant RSL as run_dialog_state_loop
    participant OU as on_update<br/>(forward_update 闭包)
    participant RT as routes
    participant HI as handle_incoming
    participant Med as MediaHandles

    RIL->>+DL: match_dialog(tx)
    DL-->>-RIL: dialog
    RIL->>DL: dialog.handle()
    Note over DL: handle_reinvite() 状态切 Updated

    RSL->>+OU: DialogState::Updated(id, req, handle)
    OU->>+RT: routes.get(id)
    RT-->>-OU: update_tx
    OU->>+HI: update_tx.send((req, reply_tx))

    par 并行执行
        OU->>HI: update_tx.send((req, reply_tx))
        HI->>HI: parse_offer_all()
        HI->>HI: negotiate_answer()
        HI->>Med: renegotiate()
        Med-->>HI: 按需重启媒体
        HI-->>-OU: reply_tx.send(answer)
    and
        OU->>handle: handle.respond(200 OK, body)
    end
```

- 对端发 re-INVITE（To 带 tag，in-dialog 请求）
- `run_dialog_state_loop` 通过 `routes` 表找到对应通话任务的 `update_tx`
- 通话任务内解析 SDP + 重新协商 + 按需重启媒体
- oneshot 回复通道把 answer SDP 带回 `on_update`

---

## 7. 媒体面架构

```mermaid
flowchart LR
    subgraph 接收端["接收端 (RTP → 上屏)"]
        UDPr["udpsrc (RTP)"]
        Depay["rtph264depay"]
        Dec["avdec_h264"]
        VConv["videoconvert"]
        Sink["appsink<br/>RawVideoFrame"]
    end

    subgraph 上屏["make_frame_sink"]
        CB["回调"]
        Buf["SharedPixelBuffer"]
        Img["slint::Image"]
    end

    subgraph 发送端-音频["发送端-音频 (采集 → RTP)"]
        SrcA["wasapisrc / alsasrc"]
        QA["queue"]
        ResA["audioresample"]
        CapsA["capsfilter<br/>8kHz"]
        EncA["alawenc / mulawenc"]
        PayA["rtppcmapay / rtppcmupay"]
        UDPA["udpsink"]
    end

    subgraph 发送端-视频["发送端-视频 (采集 → RTP)"]
        SrcV["v4l2src"]
        QV["queue"]
        VSc["videoscale"]
        CapsV["capsfilter"]
        EncV["x264enc"]
        PayV["rtph264pay"]
        UDPV["udpsink"]
    end

    UDPr --> Depay --> Dec --> VConv --> Sink
    Sink --> CB --> Buf --> Img

    SrcA --> QA --> ResA --> CapsA --> EncA --> PayA --> UDPA
    SrcV --> QV --> VSc --> CapsV --> EncV --> PayV --> UDPV
```

- **接收**：`udpsrc` → depay → 解码 → `appsink` → `make_frame_sink` → slint 纹理
- **发送-音频**：`wasapisrc`（麦克风）→ 编码 → `rtppcmapay` → `udpsink`
- **发送-视频**：`v4l2src`（摄像头）→ videoscale → x264enc → `rtph264pay` → `udpsink`
- `MediaHandles` 按指纹（编码/地址/pt）比对决定是否重启管线

---

## 8. 视频上屏流程

```mermaid
sequenceDiagram
    participant Gst as GStreamer<br/>流线程
    participant Tap as make_frame_sink<br/>回调
    participant Invoke as invoke_from_event_loop
    participant SL as slint 事件循环<br/>(主线程)

    Gst->>+Tap: RawVideoFrame (RGBA)
    Tap->>Tap: copy 到 SharedPixelBuffer
    Tap->>Invoke: invoke_from_event_loop(move || {})
    Invoke->>+SL: 执行闭包
    SL->>SL: ui.set_video_frame(Image::from_rgba8(buf))
    SL-->>-Tap: 重绘完成
```

- GStreamer **流线程**执行 `make_frame_sink` 回调
- `slint::Image` 不是 Send，不能跨线程移动
- 先 copy 到 `SharedPixelBuffer`（纯像素数据，可以 Send）
- 在 slint 事件循环里包成 `Image` 设置到 UI

---

## 9. 窗口关闭流程

```mermaid
flowchart TB
    A[用户关闭窗口] --> B[windowClose 返回]
    B --> C[drop cmd_tx]
    C --> D[cmd_rx recv None]
    D --> E[run loop break]
    E --> F{slot 有通话?}
    F -->|是| G[try_send Hangup]
    F -->|否| H[跳过]
    G --> I[sleep 500ms]
    H --> J[client stop]
    I --> J
    J --> K[reg await 等注销]
    K --> L[worker 线程退出]
```

---

## 10. 关键设计决策

| 决策 | 原因 |
|------|------|
| 独立线程 + tokio runtime | Slint 事件循环占住主线程，SIP 事务需要异步 IO，在独立线程跑自己的 runtime 避免冲突 |
| Slot 占线模型 | 同时只有一路通话，新来电时如果 slot 被占用直接回 486 BusyHere，简化状态管理 |
| `Arc<Mutex<AppState>>` | Slint UI 和 worker 都需要读写 AppState，用 Arc<Mutex> 而非 Rc<RefCell> 是因为 worker 线程需要 Send |
| `UiHandle` 用 Weak | 避免循环引用导致内存泄漏，post() 失败时 warn 而不是 panic |
| 媒体单路不崩原则 | 接收器或发送器启动失败只 warn，对应路 None，通话不因一路媒体挂掉 |
| `IncomingCtx` 参数打包 | 消除 8 参数列表的 `#[allow(clippy::too_many_arguments)]`,`slot`/`routes` 只在收尾用,保留独立传参 |
| CallRoutes/UpdateMsg 下沉 ipcam-sip | re-INVITE 转发逻辑是通用 SIP 逻辑，与 call-ui 业务解耦，可被其他应用复用 |

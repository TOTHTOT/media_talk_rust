//! libnice 0.1.x C API 手写绑定.
//! 逐条对照 nice/agent.h (本机 0.1.23), 只含 ICE 验证通路用到的最小面:
//! <https://libnice.freedesktop.org/libnice/NiceAgent.html>
#![allow(dead_code)]
// reason: 仅 nice_agent_set_remote_credentials 暂未引用 (blob SDP 路径不经过它),
// 保留供后续直喂 credentials 的场景; 逐个标注 allow 噪音过大.

use std::ffi::{c_char, c_int, c_uint, c_void};

/// Opaque NiceAgent (GObject 子类). 释放走 `g_object_unref`.
#[repr(C)]
pub(crate) struct NiceAgent {
    _private: [u8; 0],
}

/// NiceCandidate 不透明句柄: 只经 get_selected_pair 拿到指针并读首字段,
/// 布局私有于 libnice (candidate.h struct _NiceCandidate), 本 crate 不复制.
#[repr(C)]
pub(crate) struct NiceCandidate {
    _priv: [u8; 0],
}

// NiceCandidateType 枚举值 (candidate.h: HOST=0, SERVER_REFLEXIVE=1,
// PEER_REFLEXIVE=2, RELAYED=3).
pub(crate) const CAND_HOST: c_uint = 0;
pub(crate) const CAND_SRFLX: c_uint = 1;
pub(crate) const CAND_PRFLX: c_uint = 2;
pub(crate) const CAND_RELAY: c_uint = 3;

/// 读 NiceCandidate 首字段 `type` — 布局依据: candidate.h 中
/// `struct _NiceCandidate { NiceCandidateType type; ... }` type 为第一成员.
///
/// # Safety
/// `c` 必须指向 libnice 返回的合法 NiceCandidate (本 crate 内仅
/// get_selected_pair 出参, 已判空).
pub(crate) unsafe fn candidate_type(c: *mut NiceCandidate) -> c_uint {
    // SAFETY: 见函数文档; 首字段投影不依赖后续布局.
    unsafe { *c.cast::<c_uint>() }
}

/// NiceCompatibility::NICE_COMPATIBILITY_RFC5245 (agent.h 实测值 0)
pub(crate) const COMPAT_RFC5245: c_uint = 0;
/// NiceComponentState (agent.h 实测)
pub(crate) const STATE_GATHERING: c_uint = 1;
pub(crate) const STATE_CONNECTING: c_uint = 2;
pub(crate) const STATE_CONNECTED: c_uint = 3;
pub(crate) const STATE_READY: c_uint = 4;
pub(crate) const STATE_FAILED: c_uint = 5;

#[link(name = "nice")]
unsafe extern "C" {
    #[link_name = "g_object_unref"]
    pub(crate) fn g_object_unref(object: *mut c_void);

    pub(crate) fn nice_agent_new(
        ctx: *mut glib::ffi::GMainContext,
        compat: c_uint,
    ) -> *mut NiceAgent;
    pub(crate) fn nice_agent_add_stream(agent: *mut NiceAgent, n_components: c_uint) -> c_uint;
    pub(crate) fn nice_agent_gather_candidates(agent: *mut NiceAgent, stream: c_uint) -> c_int;
    pub(crate) fn nice_agent_set_remote_credentials(
        agent: *mut NiceAgent,
        stream: c_uint,
        ufrag: *const c_char,
        pwd: *const c_char,
    ) -> c_int;
    pub(crate) fn nice_agent_generate_local_sdp(agent: *mut NiceAgent) -> *mut c_char;
    pub(crate) fn nice_agent_parse_remote_sdp(agent: *mut NiceAgent, sdp: *const c_char) -> c_int;
    pub(crate) fn nice_agent_send(
        agent: *mut NiceAgent,
        stream: c_uint,
        component: c_uint,
        len: c_uint,
        buf: *const c_char,
    ) -> c_int;
    /// NiceAgentRecvFunc: (agent, stream, component, len, buf, user_data)
    pub(crate) fn nice_agent_attach_recv(
        agent: *mut NiceAgent,
        stream: c_uint,
        component: c_uint,
        ctx: *mut glib::ffi::GMainContext,
        func: Option<
            unsafe extern "C" fn(*mut NiceAgent, c_uint, c_uint, c_uint, *mut c_char, *mut c_void),
        >,
        data: *mut c_void,
    ) -> c_int;
    pub(crate) fn nice_agent_get_selected_pair(
        agent: *mut NiceAgent,
        stream: c_uint,
        component: c_uint,
        local: *mut *mut NiceCandidate,
        remote: *mut *mut NiceCandidate,
    ) -> c_int;
}

/// 连接 `component-state-changed` 信号. 回调在 agent 的 glib 线程触发,
/// 实现方只能做无锁的 channel 发送 (见 agent.rs 纪律注释).
///
/// # Safety
/// * `agent` 必须是 `nice_agent_new` 返回的存活指针.
pub(crate) unsafe fn connect_state_changed<F>(agent: *mut NiceAgent, f: F)
where
    F: Fn(u32, u32, u32) + Send + 'static,
{
    unsafe extern "C" fn trampoline<F: Fn(u32, u32, u32)>(
        _agent: *mut NiceAgent,
        stream: c_uint,
        component: c_uint,
        state: c_uint,
        data: *mut c_void,
    ) {
        // SAFETY: data 由本函数 Box::into_raw, connect_raw 注册的 destroy_closure
        // 在断开时回收 Box, 存活期覆盖所有回调, 回调只读.
        let f = unsafe { &*(data as *const F) };
        f(stream, component, state);
    }

    let data = Box::into_raw(Box::new(f));
    // SAFETY: agent 是 GObject; 信号名以 NUL 结尾; trampoline::<F> 签名与
    // NiceAgent::component-state-changed 的 (u,u,u) 参数序一致;
    // connect_raw 接管 data 的所有权并在信号断开时回收.
    unsafe {
        glib::signal::connect_raw(
            agent.cast(),
            c"component-state-changed".as_ptr(),
            // reason: connect_raw 需要 GCallback (类型擦除的 fn 指针), 裸指针
            // 到 fn 指针没有合法 as cast, 只能 transmute.
            Some(std::mem::transmute::<*const (), unsafe extern "C" fn()>(
                trampoline::<F> as *const (),
            )),
            data,
        );
    }
}

/// 连接 `candidate-gathering-done` 信号 (回调签名 ()), 结构同上.
///
/// # Safety
/// * `agent` 必须是 `nice_agent_new` 返回的存活指针.
pub(crate) unsafe fn connect_gathering_done<F>(agent: *mut NiceAgent, f: F)
where
    F: Fn() + Send + 'static,
{
    unsafe extern "C" fn trampoline<F: Fn()>(
        _agent: *mut NiceAgent,
        _stream: c_uint,
        data: *mut c_void,
    ) {
        // SAFETY: 同 connect_state_changed.
        let f = unsafe { &*(data as *const F) };
        f();
    }

    let data = Box::into_raw(Box::new(f));
    // SAFETY: 同上.
    unsafe {
        glib::signal::connect_raw(
            agent.cast(),
            c"candidate-gathering-done".as_ptr(),
            // reason: 同 connect_state_changed.
            Some(std::mem::transmute::<*const (), unsafe extern "C" fn()>(
                trampoline::<F> as *const (),
            )),
            data,
        );
    }
}

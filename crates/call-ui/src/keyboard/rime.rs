//! librime 中文引擎实现 (仅 Linux 编译).
//!
//! 直接绑定系统 librime 1.5 (focal apt 的 librime-dev), 不新增 crate 依赖.
//! 下列 FFI 结构体逐字段对照 librime 1.5.3 的 src/rime_api.h 翻译, 字段
//! 顺序与类型严格对齐; RimeApi 只声明到 select_candidate 为止的前缀
//! (之后的字段用不到, 但前缀字段必须逐一对齐, 否则函数指针错位).
//!
//! librime 非线程安全: 本引擎只被键盘事件线程 (slint UI 线程) 经控制器
//! 单线程消费, 满足其调用契约, 不做跨线程共享.
//!
//! 初始化失败 (无 librime / 输入法数据缺失 / 会话创建失败) 时降级为
//! stub 直通引擎并告警一次, 保证键盘始终可用.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use super::engine::{Engine, EngineOutput, Mode};

/// librime Bool: C int, 0 假非 0 真.
type Bool = c_int;

/// librime RimeSessionId: uintptr_t.
type RimeSessionId = usize;

/// librime 通知回调签名 (不启用通知, 仅为 RimeApi 字段类型完整).
type RimeNotificationHandler = unsafe extern "C" fn(
    context_object: *mut c_void,
    session_id: RimeSessionId,
    message_type: *const c_char,
    message_value: *const c_char,
);

/// RIME_STRUCT_INIT 语义: data_size = sizeof(Type) - sizeof(int).
fn rime_data_size<T>() -> c_int {
    (std::mem::size_of::<T>() - std::mem::size_of::<c_int>()) as c_int
}

/// RimeTraits (rime_api.h: rime_traits_t), 只用于 setup/initialize.
#[repr(C)]
struct RimeTraits {
    data_size: c_int,
    shared_data_dir: *const c_char,
    user_data_dir: *const c_char,
    distribution_name: *const c_char,
    distribution_code_name: *const c_char,
    distribution_version: *const c_char,
    app_name: *const c_char,
    modules: *const *const c_char,
}

/// RimeComposition (rime_api.h): 预编辑串与光标/选区.
#[repr(C)]
struct RimeComposition {
    length: c_int,
    cursor_pos: c_int,
    sel_start: c_int,
    sel_end: c_int,
    preedit: *mut c_char,
}

/// RimeCandidate (rime_api.h: rime_candidate_t).
#[repr(C)]
struct RimeCandidate {
    text: *mut c_char,
    comment: *mut c_char,
    reserved: *mut c_void,
}

/// RimeMenu (rime_api.h): 候选页.
#[repr(C)]
struct RimeMenu {
    page_size: c_int,
    page_no: c_int,
    is_last_page: Bool,
    highlighted_candidate_index: c_int,
    num_candidates: c_int,
    candidates: *mut RimeCandidate,
    select_keys: *mut c_char,
}

/// RimeCommit (rime_api.h: rime_commit_t), 上屏文本.
#[repr(C)]
struct RimeCommit {
    data_size: c_int,
    text: *mut c_char,
}

/// RimeContext (rime_api.h: rime_context_t): 组合状态 + 候选菜单.
#[repr(C)]
struct RimeContext {
    data_size: c_int,
    composition: RimeComposition,
    menu: RimeMenu,
    commit_text_preview: *mut c_char,
    select_labels: *mut *mut c_char,
}

/// RimeStatus (rime_api.h: rime_status_t), 仅作 RimeApi 字段类型.
#[repr(C)]
struct RimeStatus {
    data_size: c_int,
    schema_id: *mut c_char,
    schema_name: *mut c_char,
    is_disabled: Bool,
    is_composing: Bool,
    is_ascii_mode: Bool,
    is_full_shape: Bool,
    is_simplified: Bool,
    is_traditional: Bool,
    is_ascii_punct: Bool,
}

/// RimeConfig (rime_api.h: rime_config_t), 仅作 RimeApi 字段类型.
#[repr(C)]
struct RimeConfig {
    ptr: *mut c_void,
}

/// RimeConfigIterator (rime_api.h), 仅作 RimeApi 字段类型.
#[repr(C)]
struct RimeConfigIterator {
    list: *mut c_void,
    map: *mut c_void,
    index: c_int,
    key: *const c_char,
    path: *const c_char,
}

/// RimeSchemaListItem (rime_api.h), 仅作 RimeApi 字段类型.
#[repr(C)]
struct RimeSchemaListItem {
    schema_id: *mut c_char,
    name: *mut c_char,
    reserved: *mut c_void,
}

/// RimeSchemaList (rime_api.h), 仅作 RimeApi 字段类型.
#[repr(C)]
struct RimeSchemaList {
    size: usize,
    list: *mut RimeSchemaListItem,
}

/// RimeCustomApi (rime_api.h: rime_custom_api_t), 仅作 RimeModule 字段类型.
#[repr(C)]
struct RimeCustomApi {
    data_size: c_int,
}

/// RimeModule (rime_api.h: rime_module_t), 仅作 RimeApi 字段类型.
#[repr(C)]
struct RimeModule {
    data_size: c_int,
    module_name: *const c_char,
    initialize: Option<unsafe extern "C" fn()>,
    finalize: Option<unsafe extern "C" fn()>,
    get_api: Option<unsafe extern "C" fn() -> *mut RimeCustomApi>,
}

/// RimeApi (rime_api.h: rime_api_t) 前缀: data_size 到 select_candidate.
///
/// 逐字段对照 1.5.3 头文件顺序, 不可重排/省略前缀中任何字段.
#[repr(C)]
struct RimeApi {
    data_size: c_int,
    setup: Option<unsafe extern "C" fn(*mut RimeTraits)>,
    set_notification_handler: Option<unsafe extern "C" fn(RimeNotificationHandler, *mut c_void)>,
    initialize: Option<unsafe extern "C" fn(*mut RimeTraits)>,
    finalize: Option<unsafe extern "C" fn()>,
    start_maintenance: Option<unsafe extern "C" fn(Bool) -> Bool>,
    is_maintenance_mode: Option<unsafe extern "C" fn() -> Bool>,
    join_maintenance_thread: Option<unsafe extern "C" fn()>,
    deployer_initialize: Option<unsafe extern "C" fn(*mut RimeTraits)>,
    prebuild: Option<unsafe extern "C" fn() -> Bool>,
    deploy: Option<unsafe extern "C" fn() -> Bool>,
    deploy_schema: Option<unsafe extern "C" fn(*const c_char) -> Bool>,
    deploy_config_file: Option<unsafe extern "C" fn(*const c_char, *const c_char) -> Bool>,
    sync_user_data: Option<unsafe extern "C" fn() -> Bool>,
    create_session: Option<unsafe extern "C" fn() -> RimeSessionId>,
    find_session: Option<unsafe extern "C" fn(RimeSessionId) -> Bool>,
    destroy_session: Option<unsafe extern "C" fn(RimeSessionId) -> Bool>,
    cleanup_stale_sessions: Option<unsafe extern "C" fn()>,
    cleanup_all_sessions: Option<unsafe extern "C" fn()>,
    process_key: Option<unsafe extern "C" fn(RimeSessionId, c_int, c_int) -> Bool>,
    commit_composition: Option<unsafe extern "C" fn(RimeSessionId) -> Bool>,
    clear_composition: Option<unsafe extern "C" fn(RimeSessionId)>,
    get_commit: Option<unsafe extern "C" fn(RimeSessionId, *mut RimeCommit) -> Bool>,
    free_commit: Option<unsafe extern "C" fn(*mut RimeCommit) -> Bool>,
    get_context: Option<unsafe extern "C" fn(RimeSessionId, *mut RimeContext) -> Bool>,
    free_context: Option<unsafe extern "C" fn(*mut RimeContext) -> Bool>,
    get_status: Option<unsafe extern "C" fn(RimeSessionId, *mut RimeStatus) -> Bool>,
    free_status: Option<unsafe extern "C" fn(*mut RimeStatus) -> Bool>,
    set_option: Option<unsafe extern "C" fn(RimeSessionId, *const c_char, Bool)>,
    get_option: Option<unsafe extern "C" fn(RimeSessionId, *const c_char) -> Bool>,
    set_property: Option<unsafe extern "C" fn(RimeSessionId, *const c_char, *const c_char)>,
    get_property:
        Option<unsafe extern "C" fn(RimeSessionId, *const c_char, *mut c_char, usize) -> Bool>,
    get_schema_list: Option<unsafe extern "C" fn(*mut RimeSchemaList) -> Bool>,
    free_schema_list: Option<unsafe extern "C" fn(*mut RimeSchemaList)>,
    get_current_schema: Option<unsafe extern "C" fn(RimeSessionId, *mut c_char, usize) -> Bool>,
    select_schema: Option<unsafe extern "C" fn(RimeSessionId, *const c_char) -> Bool>,
    schema_open: Option<unsafe extern "C" fn(*const c_char, *mut RimeConfig) -> Bool>,
    config_open: Option<unsafe extern "C" fn(*const c_char, *mut RimeConfig) -> Bool>,
    config_close: Option<unsafe extern "C" fn(*mut RimeConfig) -> Bool>,
    config_get_bool:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, *mut Bool) -> Bool>,
    config_get_int:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, *mut c_int) -> Bool>,
    config_get_double:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, *mut f64) -> Bool>,
    config_get_string:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, *mut c_char, usize) -> Bool>,
    config_get_cstring:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char) -> *const c_char>,
    config_update_signature: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char) -> Bool>,
    config_begin_map: Option<
        unsafe extern "C" fn(*mut RimeConfigIterator, *mut RimeConfig, *const c_char) -> Bool,
    >,
    config_next: Option<unsafe extern "C" fn(*mut RimeConfigIterator) -> Bool>,
    config_end: Option<unsafe extern "C" fn(*mut RimeConfigIterator)>,
    simulate_key_sequence: Option<unsafe extern "C" fn(RimeSessionId, *const c_char) -> Bool>,
    register_module: Option<unsafe extern "C" fn(*mut RimeModule) -> Bool>,
    find_module: Option<unsafe extern "C" fn(*const c_char) -> *mut RimeModule>,
    run_task: Option<unsafe extern "C" fn(*const c_char) -> Bool>,
    get_shared_data_dir: Option<unsafe extern "C" fn() -> *const c_char>,
    get_user_data_dir: Option<unsafe extern "C" fn() -> *const c_char>,
    get_sync_dir: Option<unsafe extern "C" fn() -> *const c_char>,
    get_user_id: Option<unsafe extern "C" fn() -> *const c_char>,
    get_user_data_sync_dir: Option<unsafe extern "C" fn(*mut c_char, usize)>,
    config_init: Option<unsafe extern "C" fn(*mut RimeConfig) -> Bool>,
    config_load_string: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char) -> Bool>,
    config_set_bool: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, Bool) -> Bool>,
    config_set_int: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, c_int) -> Bool>,
    config_set_double: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, f64) -> Bool>,
    config_set_string:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, *const c_char) -> Bool>,
    config_get_item:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, *mut RimeConfig) -> Bool>,
    config_set_item:
        Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char, *mut RimeConfig) -> Bool>,
    config_clear: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char) -> Bool>,
    config_create_list: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char) -> Bool>,
    config_create_map: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char) -> Bool>,
    config_list_size: Option<unsafe extern "C" fn(*mut RimeConfig, *const c_char) -> usize>,
    config_begin_list: Option<
        unsafe extern "C" fn(*mut RimeConfigIterator, *mut RimeConfig, *const c_char) -> Bool,
    >,
    get_input: Option<unsafe extern "C" fn(RimeSessionId) -> *const c_char>,
    get_caret_pos: Option<unsafe extern "C" fn(RimeSessionId) -> usize>,
    select_candidate: Option<unsafe extern "C" fn(RimeSessionId, usize) -> Bool>,
}

#[link(name = "rime")]
unsafe extern "C" {
    fn rime_get_api() -> *const RimeApi;
}

/// librime api 指针单例; 0 表示初始化失败 (用 usize 避开原始指针的非 Sync 约束).
static API: OnceLock<usize> = OnceLock::new();

/// 降级告警只报一次.
static FALLBACK_WARNED: AtomicBool = AtomicBool::new(false);

/// 候选栏最多展示的候选数.
const MAX_CANDIDATES: usize = 8;

/// X11 keysym: BackSpace (librime keycode 用 X keysym 值).
const XK_BACK_SPACE: c_int = 0xFF08;

/// 取 librime api 单例; 首次调用做完整初始化, 失败缓存 0 不重试.
fn api() -> Option<&'static RimeApi> {
    let ptr = *API.get_or_init(init_api);
    if ptr == 0 {
        return None;
    }
    // SAFETY: 非 0 仅在 init_api 从 rime_get_api() 拿到非空指针后写入;
    // 该指针随动态库加载常驻, OnceLock 保证写后读安全.
    Some(unsafe { &*(ptr as *const RimeApi) })
}

/// librime 初始化流程入口 (api() 的 OnceLock 初始化闭包):
/// 1. rime_get_api() 取 api 结构, 校验所需函数指针非空
/// 2. 组装 RimeTraits (shared_data_dir 取 RIME_DATA_DIR, 默认
///    /usr/share/rime-data; user_data_dir 取 ~/.local/share/media_talk/rime)
/// 3. setup + initialize (traits 传空, 用 setup 缓存值)
/// 4. start_maintenance(0) 后轮询 is_maintenance_mode, 100ms 步进, 10s 超时
///
/// 返回 api 指针; 任一步失败返回 0.
fn init_api() -> usize {
    // SAFETY: rime_get_api 无参, 库加载后即可调用, 返回静态 RimeApi 指针.
    let api = unsafe { rime_get_api() };
    // SAFETY: 判空后才解引用.
    let Some(api) = (unsafe { api.as_ref() }) else {
        tracing::warn!("rime_get_api() returned null, librime missing?");
        return 0;
    };
    macro_rules! required {
        ($($f:ident),+ $(,)?) => {
            $(
                if api.$f.is_none() {
                    tracing::warn!(func = stringify!($f), "librime api function missing");
                    return 0;
                }
            )+
        };
    }
    required!(
        setup,
        initialize,
        start_maintenance,
        is_maintenance_mode,
        create_session,
        process_key,
        clear_composition,
        get_context,
        free_context,
        get_commit,
        free_commit,
        select_candidate,
    );

    let shared_dir = std::env::var("RIME_DATA_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/usr/share/rime-data".to_string());
    let user_dir = directories::BaseDirs::new()
        .map(|b| b.data_dir().join("media_talk").join("rime"))
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp/media_talk/rime"));
    if let Err(err) = std::fs::create_dir_all(&user_dir) {
        tracing::warn!(?err, dir = %user_dir.display(), "cannot create rime user data dir");
    }
    let Ok(shared_c) = CString::new(shared_dir) else {
        return 0;
    };
    let Ok(user_c) = CString::new(user_dir.to_string_lossy().as_ref()) else {
        return 0;
    };

    // SAFETY: 全字段为整数/指针, 零初始化合法; 随后按 RIME_STRUCT 语义填
    // data_size; 各字符串指针在本函数作用域内存活至维护结束 (librime 对
    // 目录字符串做内部拷贝, 但部署期仍读取, 不能提前释放).
    let mut traits = unsafe { std::mem::zeroed::<RimeTraits>() };
    traits.data_size = rime_data_size::<RimeTraits>();
    traits.shared_data_dir = shared_c.as_ptr();
    traits.user_data_dir = user_c.as_ptr();
    traits.distribution_name = c"media_talk".as_ptr();
    traits.distribution_code_name = c"media_talk".as_ptr();
    traits.distribution_version = c"0.1".as_ptr();
    traits.app_name = c"media_talk.rime".as_ptr();

    // SAFETY: traits 为按 RIME_STRUCT 语义初始化的栈结构, 指针字段均有效.
    unsafe {
        if let Some(setup) = api.setup {
            setup(&mut traits);
        }
        if let Some(initialize) = api.initialize {
            initialize(std::ptr::null_mut());
        }
    }

    // 部署输入法数据 (首次运行生成用户目录配置), 轮询等待完成; 一次性
    // 阻塞, 最长 10s, 仅发生在首次键盘初始化.
    if let Some(start_maintenance) = api.start_maintenance {
        // SAFETY: 初始化已完成, 无参调用安全.
        unsafe { start_maintenance(0) };
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let busy = api.is_maintenance_mode.is_some_and(|f| unsafe { f() } != 0);
        if !busy {
            break;
        }
        if std::time::Instant::now() >= deadline {
            tracing::warn!("rime maintenance not finished in 10s, continuing anyway");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    api as *const RimeApi as usize
}

/// 创建引擎; librime 不可用时降级为直通引擎 (stub) 并告警一次.
pub(crate) fn create_engine() -> Box<dyn Engine> {
    match RimeEngine::new() {
        Some(engine) => Box::new(engine),
        None => {
            if !FALLBACK_WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!("librime unavailable, keyboard falls back to direct output");
            }
            super::stub::create_engine()
        }
    }
}

/// librime 会话引擎: 每个会话一个 RimeSessionId, 预编辑/候选每次现取.
struct RimeEngine {
    session: RimeSessionId,
    mode: Mode,
}

impl RimeEngine {
    fn new() -> Option<Self> {
        let api = api()?;
        let create_session = api.create_session?;
        // SAFETY: 初始化已完成 (api() 保证), 无参调用安全.
        let session = unsafe { create_session() };
        if session == 0 {
            tracing::warn!("rime_create_session() returned 0");
            return None;
        }
        Some(Self {
            session,
            mode: Mode::Zh,
        })
    }

    /// 现取快照: 预编辑串 + 截断 8 个候选 + 取走待上屏提交.
    fn output(&self) -> EngineOutput {
        let mut out = EngineOutput::default();
        let Some(api) = api() else {
            return out;
        };
        let mut ctx = RimeContext::new();
        // SAFETY: session 有效; ctx 按 RIME_STRUCT 语义初始化, librime 按
        // data_size 感知结构版本, 只填充不读其余字段.
        let ok = unsafe {
            match api.get_context {
                Some(get_context) => get_context(self.session, &mut ctx),
                None => 0,
            }
        };
        if ok != 0 {
            out.preedit = cstr_to_string(ctx.composition.preedit);
            let menu = &ctx.menu;
            let count = menu.num_candidates.max(0) as usize;
            if count > 0 && !menu.candidates.is_null() {
                for i in 0..count.min(MAX_CANDIDATES) {
                    // SAFETY: candidates 指向 librime 分配的 num_candidates
                    // 长数组, i < count, 在界内.
                    let candidate = unsafe { &*menu.candidates.add(i) };
                    out.candidates.push(cstr_to_string(candidate.text));
                }
            }
        }
        // SAFETY: ctx 无论填充成败都按 RIME_STRUCT 语义分配过, 必须配对释放.
        unsafe {
            if let Some(free_context) = api.free_context {
                free_context(&mut ctx);
            }
        }
        out.commits = self.drain_commits();
        out
    }

    /// 循环取走 rime 产生的上屏文本, 直到没有待提交内容.
    fn drain_commits(&self) -> Vec<String> {
        let mut commits = Vec::new();
        let Some(api) = api() else {
            return commits;
        };
        loop {
            let mut commit = RimeCommit::new();
            // SAFETY: session 有效; commit 按 RIME_STRUCT 语义初始化.
            let ok = unsafe {
                match api.get_commit {
                    Some(get_commit) => get_commit(self.session, &mut commit),
                    None => 0,
                }
            };
            if ok == 0 {
                break;
            }
            commits.push(cstr_to_string(commit.text));
            // SAFETY: commit 已由 get_commit 填充, 需配对释放防泄漏.
            unsafe {
                if let Some(free_commit) = api.free_commit {
                    free_commit(&mut commit);
                }
            }
        }
        commits
    }

    /// 读取当前预编辑串 (现取, 不缓存).
    fn preedit(&self) -> String {
        let Some(api) = api() else {
            return String::new();
        };
        let mut ctx = RimeContext::new();
        // SAFETY: 同 output(), 只取 composition.preedit 后立即释放.
        let ok = unsafe {
            match api.get_context {
                Some(get_context) => get_context(self.session, &mut ctx),
                None => 0,
            }
        };
        let preedit = if ok != 0 {
            cstr_to_string(ctx.composition.preedit)
        } else {
            String::new()
        };
        // SAFETY: 同 output(), 配对释放.
        unsafe {
            if let Some(free_context) = api.free_context {
                free_context(&mut ctx);
            }
        }
        preedit
    }

    /// 清空组合 (模式切换时调用, 丢弃未上屏的预编辑).
    fn clear_composition(&self) {
        let Some(api) = api() else {
            return;
        };
        // SAFETY: session 有效.
        unsafe {
            if let Some(clear_composition) = api.clear_composition {
                clear_composition(self.session);
            }
        }
    }
}

impl RimeCommit {
    /// 按 RIME_STRUCT 语义零初始化并填 data_size.
    fn new() -> Self {
        // SAFETY: 全字段为整数/指针, 零初始化合法.
        let mut commit = unsafe { std::mem::zeroed::<Self>() };
        commit.data_size = rime_data_size::<Self>();
        commit
    }
}

impl RimeContext {
    /// 按 RIME_STRUCT 语义零初始化并填 data_size.
    fn new() -> Self {
        // SAFETY: 全字段为整数/指针/定长结构, 零初始化合法.
        let mut ctx = unsafe { std::mem::zeroed::<Self>() };
        ctx.data_size = rime_data_size::<Self>();
        ctx
    }
}

/// C 字符串转 String; 空指针返回空串 (FFI 字段可能为 NULL).
fn cstr_to_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: 调用方保证 ptr 指向 librime 分配的合法 NUL 结尾字符串
    // (librime 契约), 且仅在 free_context/free_commit 前访问.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

impl Engine for RimeEngine {
    fn mode(&self) -> Mode {
        self.mode
    }

    fn set_mode(&mut self, mode: Mode) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.clear_composition();
    }

    fn key(&mut self, k: char) -> EngineOutput {
        let direct_commit = match self.mode {
            // 中文模式: 小写字母进引擎累积拼音, 其余 (标点/数字/大写) 直出
            Mode::Zh => !k.is_ascii_lowercase(),
            Mode::En | Mode::Num => true,
        };
        if direct_commit {
            return EngineOutput {
                commits: vec![k.to_string()],
                ..Default::default()
            };
        }
        if let Some(api) = api() {
            if let Some(process_key) = api.process_key {
                // SAFETY: session 有效; keycode 为 ASCII 小写字母值, mask 0.
                unsafe { process_key(self.session, k as c_int, 0) };
            }
        }
        self.output()
    }

    fn backspace(&mut self) -> EngineOutput {
        if self.preedit().is_empty() {
            // 预编辑已空: 退格穿透到输入框, 不再进引擎
            return EngineOutput {
                delete_field: true,
                ..Default::default()
            };
        }
        if let Some(api) = api() {
            if let Some(process_key) = api.process_key {
                // SAFETY: session 有效; XK_BackSpace 为 librime 约定的退格键码.
                unsafe { process_key(self.session, XK_BACK_SPACE, 0) };
            }
        }
        self.output()
    }

    fn select(&mut self, idx: usize) -> EngineOutput {
        if let Some(api) = api() {
            if let Some(select_candidate) = api.select_candidate {
                // SAFETY: session 有效; idx 超出候选数时 librime 返回假, 无副作用.
                unsafe { select_candidate(self.session, idx) };
            }
        }
        self.output()
    }

    fn space(&mut self) -> EngineOutput {
        if self.mode == Mode::Zh && !self.preedit().is_empty() {
            self.select(0)
        } else {
            EngineOutput {
                commits: vec![" ".to_string()],
                ..Default::default()
            }
        }
    }
}

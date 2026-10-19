//! 摄像头功能: 设置页管理 (搜索/添加/删除) + 监控页查看控制 (Task 6).
//! ONVIF 搜索是异步网络操作, 在独立线程跑 tokio runtime, 结果经
//! invoke_from_event_loop 回 UI 线程刷属性, UI 线程零阻塞.

use std::cell::RefCell;
use std::rc::Rc;

use ipcam_gst::VideoFrameSink;
use slint::ComponentHandle;
use tracing::{error, warn};

use crate::MainWindow;
use crate::props;
use crate::store::Store;

/// 设备列表行文案: 有厂商/型号拼 "厂商 型号 (地址)", 都没有退化为地址
fn device_label(d: &ipcam_core::DiscoveredDevice) -> String {
    match (&d.manufacturer, &d.model) {
        (Some(m), Some(md)) => format!("{m} {md} ({})", d.address),
        _ => d.address.clone(),
    }
}

/// profile 行文案: "H264 1920x1080"; 未知分辨率只写编码
fn profile_label(p: &ipcam_core::VideoProfile) -> String {
    if p.width > 0 {
        format!("{:?} {}x{}", p.codec, p.width, p.height)
    } else {
        format!("{:?}", p.codec)
    }
}

/// 设置页摄像头管理: CRUD 全量重刷 (通讯录模式) + ONVIF 搜索线程桥
pub(crate) fn wire_settings(ui: &MainWindow, store: &Rc<Store>) {
    props::refresh_cameras(ui, store);

    // 手动添加
    {
        let (store, weak) = (store.clone(), ui.as_weak());
        ui.on_add_camera(move |name, uri, user, pass| {
            if let Err(e) = store.add_camera(&name, &uri, &user, &pass) {
                error!(err = %e, "add camera failed");
            }
            if let Some(ui) = weak.upgrade() {
                props::refresh_cameras(&ui, &store);
            }
        });
    }
    // 删除
    {
        let (store, weak) = (store.clone(), ui.as_weak());
        ui.on_delete_camera(move |id| {
            if let Err(e) = store.delete_camera(id as i64) {
                error!(err = %e, "delete camera failed");
            }
            if let Some(ui) = weak.upgrade() {
                props::refresh_cameras(&ui, &store);
            }
        });
    }
    // ONVIF 搜索: busy 置位 → 线程搜索 → 回刷设备列表
    {
        let weak = ui.as_weak();
        ui.on_search_cameras(move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_camera_busy(true);
            let weak = weak.clone();
            std::thread::spawn(move || {
                let devices = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map(|rt| {
                        rt.block_on(ipcam_discovery::probe_all_with_config(
                            ipcam_discovery::DiscoveryConfig::default(),
                        ))
                    })
                    .unwrap_or_default();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let entries: Vec<crate::DiscoveredEntry> = devices
                        .iter()
                        .enumerate()
                        .map(|(i, d)| crate::DiscoveredEntry {
                            index: i as i32,
                            label: device_label(d).into(),
                            xaddr: d.xaddr.clone().unwrap_or_default().into(),
                        })
                        .collect();
                    ui.set_discovered_devices(slint::ModelRc::new(slint::VecModel::from(entries)));
                    ui.set_camera_busy(false);
                });
            });
        });
    }
    // 拉 profile: 选中设备 + 凭据 → DeviceManagementClient
    {
        let weak = ui.as_weak();
        ui.on_fetch_camera_profiles(move |xaddr, user, pass| {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_camera_busy(true);
            // 清空上次结果, busy 期间步骤 2 显示"正在读取"
            let empty: Vec<crate::ProfileEntry> = Vec::new();
            ui.set_camera_profiles(slint::ModelRc::new(slint::VecModel::from(empty)));
            let weak = weak.clone();
            std::thread::spawn(move || {
                let profiles = fetch_profiles_blocking(&xaddr, &user, &pass);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    match profiles {
                        Ok(list) => {
                            let entries: Vec<crate::ProfileEntry> = list
                                .iter()
                                .map(|p| crate::ProfileEntry {
                                    token: p.profile_id.clone().into(),
                                    label: profile_label(p).into(),
                                })
                                .collect();
                            ui.set_camera_profiles(slint::ModelRc::new(slint::VecModel::from(
                                entries,
                            )));
                        }
                        Err(e) => warn!(error = %e, "fetch camera profiles failed"),
                    }
                    ui.set_camera_busy(false);
                });
            });
        });
    }
    // 保存搜索到的设备: 取 profile 的 RTSP URI (profile 自带 uri 优先,
    // 否则 get_stream_uri 现取) → 入库 → 重刷列表 + toast.
    // Rc<Store> 不能跨线程 (invoke_from_event_loop 要求 Send), worker
    // 线程自开一条连接写库, 只把最新列表 (纯数据) 带回 UI 线程
    {
        let weak = ui.as_weak();
        ui.on_save_discovered_camera(move |xaddr, name, user, pass, token| {
            let weak = weak.clone();
            std::thread::spawn(move || {
                let saved: anyhow::Result<Vec<crate::store::CameraRow>> =
                    save_discovered_blocking(&xaddr, &user, &pass, &token)
                        .map_err(|e| anyhow::anyhow!(e))
                        .and_then(|uri| {
                            let store = Store::open()?;
                            store.add_camera(&name, &uri, &user, &pass)?;
                            store.cameras()
                        });
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    match saved {
                        Ok(list) => {
                            props::set_camera_entries(&ui, &list);
                            ui.invoke_notify(0, 0, slint::SharedString::from("摄像头已保存"));
                        }
                        Err(e) => {
                            warn!(error = %e, "save discovered camera failed");
                            ui.invoke_notify(
                                1,
                                0,
                                slint::SharedString::from(format!("保存失败: {e}")),
                            );
                        }
                    }
                });
            });
        });
    }
}

/// 监控查看控制器: 任一时刻最多一路 ingest 在跑 (单路全屏形态).
/// 与对讲共用同一个 tap/video-frame 属性 — 靠"phase==0 才能进监控页
/// + 拨打/接听时强制 stop"保证不并发
#[derive(Default)]
pub(crate) struct MonitorController {
    current: Option<ipcam_gst::GstStreamHandle>,
    selected: Option<i64>,
}

impl MonitorController {
    pub fn new() -> Self {
        Self::default()
    }

    // 当前选中摄像头 id; 目前只有状态机单测消费 (UI 侧选中态走
    // monitor-selected-id 属性), 故仅测试构建可见
    #[cfg(test)]
    pub fn selected_id(&self) -> Option<i64> {
        self.selected
    }

    /// 选中摄像头: 停旧起新 (本地查看模式 + 限宽 640, 不要摄像头音频).
    /// 启动失败 (RTSP 不通/认证错) 时旧流已停, 返回 Err 由调用方 toast
    pub fn select(
        &mut self,
        cam: &crate::store::CameraRow,
        tap: &VideoFrameSink,
    ) -> Result<(), ipcam_gst::GstStreamError> {
        self.stop();
        let cfg = ipcam_gst::GstStreamConfig {
            uri: cam.uri.clone(),
            credentials: if cam.username.is_empty() {
                None
            } else {
                Some((cam.username.clone(), cam.password.clone()))
            },
            audio_output: ipcam_gst::AudioOutput::Disabled,
            webrtc: false,
            tap_max_width: Some(640),
            // stream_name 本地模式用不到 (不建 webrtcsink), 随便给个唯一的
            stream_name: format!("monitor-{}", cam.id),
            ..Default::default()
        };
        let taps = ipcam_gst::RawTaps {
            video: Some(tap.clone()),
            audio: None,
        };
        let handle = ipcam_gst::start_with_taps(cfg, taps)?;
        self.current = Some(handle);
        self.selected = Some(cam.id);
        Ok(())
    }

    /// 停止当前查看 (幂等)
    pub fn stop(&mut self) {
        if let Some(h) = self.current.take() {
            h.stop();
        }
        self.selected = None;
    }
}

/// 停止监控查看并把 UI 的 monitor-selected-id 复位为 -1.
/// 停监控的所有路径 (拨打/接听抢视频通路, 离开监控页) 都要走这里,
/// 否则侧边栏会残留高亮一个已停播的摄像头
pub(crate) fn stop_and_reset_ui(
    monitor: &Rc<RefCell<MonitorController>>,
    weak: &slint::Weak<MainWindow>,
) {
    monitor.borrow_mut().stop();
    if let Some(ui) = weak.upgrade() {
        ui.set_monitor_selected_id(-1);
    }
}

/// 监控页装配: 选中查看 / 离开停止
pub(crate) fn wire_monitor(
    ui: &MainWindow,
    store: &Rc<Store>,
    monitor: &Rc<RefCell<MonitorController>>,
    tap: &VideoFrameSink,
) {
    {
        let (store, monitor, tap, weak) =
            (store.clone(), monitor.clone(), tap.clone(), ui.as_weak());
        ui.on_monitor_select(move |id| {
            let cam = store
                .cameras()
                .ok()
                .and_then(|list| list.into_iter().find(|c| c.id == id as i64));
            let Some(cam) = cam else { return };
            let mut m = monitor.borrow_mut();
            match m.select(&cam, &tap) {
                Ok(()) => {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_monitor_selected_id(id);
                    }
                }
                Err(e) => {
                    warn!(error = %e, uri = %cam.uri, "monitor select failed");
                    if let Some(ui) = weak.upgrade() {
                        ui.set_monitor_selected_id(-1);
                        ui.invoke_notify(1, 0, slint::SharedString::from("摄像头连接失败"));
                    }
                }
            }
        });
    }
    {
        let (monitor, weak) = (monitor.clone(), ui.as_weak());
        ui.on_monitor_leave(move || {
            stop_and_reset_ui(&monitor, &weak);
        });
    }
}

/// 阻塞版 profile 拉取 (独立线程 + 一次性 runtime)
fn fetch_profiles_blocking(
    xaddr: &str,
    user: &str,
    pass: &str,
) -> Result<Vec<ipcam_core::VideoProfile>, ipcam_discovery::DiscoveryError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(ipcam_discovery::DiscoveryError::Io)?;
    rt.block_on(async {
        let url = ipcam_discovery::parse_xaddr_endpoint(xaddr)?;
        let client = ipcam_discovery::DeviceManagementClient::new(
            url,
            ipcam_discovery::DiscoveryCredentials::new(user, pass),
            std::time::Duration::from_secs(5),
        );
        client.list_profiles().await
    })
}

/// 阻塞版保存: 重拉 profile 列表找到 token 对应项, 自带 uri 优先,
/// 否则 get_stream_uri 现取 (凭据此时必然有效, 上一步刚验证过)
fn save_discovered_blocking(
    xaddr: &str,
    user: &str,
    pass: &str,
    token: &str,
) -> Result<String, ipcam_discovery::DiscoveryError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(ipcam_discovery::DiscoveryError::Io)?;
    rt.block_on(async {
        let url = ipcam_discovery::parse_xaddr_endpoint(xaddr)?;
        let client = ipcam_discovery::DeviceManagementClient::new(
            url,
            ipcam_discovery::DiscoveryCredentials::new(user, pass),
            std::time::Duration::from_secs(5),
        );
        let profiles = client.list_profiles().await?;
        match profiles.iter().find(|p| p.profile_id == token) {
            Some(p) => match &p.uri {
                Some(uri) => Ok(uri.clone()),
                None => client.get_stream_uri(token).await,
            },
            None => Err(ipcam_discovery::DiscoveryError::DeviceMgmt(format!(
                "profile {token} not found on device"
            ))),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_stop_is_idempotent_and_clears_selection() {
        let mut c = MonitorController::new();
        assert!(c.selected_id().is_none());
        c.stop(); // 空状态 stop 不 panic
        assert!(c.selected_id().is_none());
    }

    #[test]
    fn device_label_prefers_manufacturer_model() {
        let d = ipcam_core::DiscoveredDevice {
            id: Default::default(), // DeviceId 是 Uuid 类型别名, nil 即可
            address: "192.168.1.64".into(),
            xaddr: None,
            scopes: vec![],
            manufacturer: Some("Hikvision".into()),
            model: Some("DS-2CD".into()),
            auth_status: ipcam_core::AuthStatus::Unknown,
            profiles: vec![],
        };
        assert_eq!(device_label(&d), "Hikvision DS-2CD (192.168.1.64)");
    }

    #[test]
    fn device_label_falls_back_to_address() {
        let d = ipcam_core::DiscoveredDevice {
            id: Default::default(),
            address: "192.168.1.64".into(),
            xaddr: None,
            scopes: vec![],
            manufacturer: None,
            model: None,
            auth_status: ipcam_core::AuthStatus::Unknown,
            profiles: vec![],
        };
        assert_eq!(device_label(&d), "192.168.1.64");
    }

    #[test]
    fn profile_label_with_and_without_resolution() {
        let p = ipcam_core::VideoProfile {
            profile_id: "p0".into(),
            uri: None,
            codec: ipcam_core::VideoCodec::H264,
            width: 1920,
            height: 1080,
            fps: 25.0,
        };
        assert_eq!(profile_label(&p), "H264 1920x1080");

        let unknown = ipcam_core::VideoProfile {
            width: 0,
            height: 0,
            ..p
        };
        assert_eq!(profile_label(&unknown), "H264");
    }
}

//! 摄像头功能: 设置页管理 (搜索/添加/删除) + 监控页查看控制 (Task 6).
//! ONVIF 搜索是异步网络操作, 在独立线程跑 tokio runtime, 结果经
//! invoke_from_event_loop 回 UI 线程刷属性, UI 线程零阻塞.

use std::rc::Rc;

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

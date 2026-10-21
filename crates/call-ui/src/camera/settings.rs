//! 设置页摄像头管理: CRUD 全量重刷 (通讯录模式) + ONVIF 搜索线程桥
//! (搜索/拉 profile/保存搜索到的设备), 以及行文案/凭据默认值等辅助.

use std::rc::Rc;

use slint::ComponentHandle;
use tracing::{error, warn};

use crate::MainWindow;
use crate::app::{NoticeKind, NoticeLevel};
use crate::props;
use crate::store::Store;

/// 设备列表行文案: 有厂商/型号拼 "厂商 型号 (地址)", 都没有退化为地址
fn device_label(d: &ipcam_core::DiscoveredDevice) -> String {
    match (&d.manufacturer, &d.model) {
        (Some(m), Some(md)) => format!("{m} {md} ({})", d.address),
        _ => d.address.clone(),
    }
}

/// profile 行文案: "H264 1920x1080"; 未知分辨率只写编码;
/// 编码也未知 (oxvif 后端没解析 VideoEncoderConfiguration 时) 退化用 profile_id,
/// 至少能区分多条 profile (实测狄耐克设备 token 形如 h264_profile/jpeg_profile)
fn profile_label(p: &ipcam_core::VideoProfile) -> String {
    if matches!(p.codec, ipcam_core::VideoCodec::Unknown) {
        return p.profile_id.clone();
    }
    if p.width > 0 {
        format!("{:?} {}x{}", p.codec, p.width, p.height)
    } else {
        format!("{:?}", p.codec)
    }
}

/// 空凭据补出厂默认 admin/123456: 输入框只给 placeholder 提示不预填,
/// 留空即默认 (placeholder 只是 UI 提示, 生效逻辑在这里)
fn creds_or_default(user: &str, pass: &str) -> (String, String) {
    (
        if user.is_empty() { "admin" } else { user }.to_string(),
        if pass.is_empty() { "123456" } else { pass }.to_string(),
    )
}

/// 设置页摄像头管理: CRUD 全量重刷 (通讯录模式) + ONVIF 搜索线程桥
pub(crate) fn wire_settings(ui: &MainWindow, store: &Rc<Store>) {
    props::refresh_cameras(ui, store);

    // 手动添加 (无 profile 信息, kind 记未知 "")
    {
        let (store, weak) = (store.clone(), ui.as_weak());
        ui.on_add_camera(move |name, uri, user, pass| {
            let (user, pass) = creds_or_default(&user, &pass);
            let r = store.add_camera(&name, &uri, &user, &pass, "");
            if let Some(ui) = weak.upgrade() {
                match r {
                    Ok(outcome) => {
                        props::refresh_cameras(&ui, &store);
                        toast_save_outcome(&ui, outcome);
                    }
                    Err(e) => {
                        error!(err = %e, "add camera failed");
                        ui.invoke_notify(
                            NoticeLevel::Warn as i32,
                            NoticeKind::CameraSaveFailed as i32,
                            e.to_string().into(),
                        );
                    }
                }
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
            let (user, pass) = creds_or_default(&user, &pass);
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
    // 否则 get_stream_uri 现取) → 入库 (同 IP 合并) → 重刷列表 + toast.
    // Rc<Store> 不能跨线程 (invoke_from_event_loop 要求 Send), worker
    // 线程自开一条连接写库, 只把最新列表 (纯数据) 带回 UI 线程
    {
        let weak = ui.as_weak();
        ui.on_save_discovered_camera(move |xaddr, name, user, pass, token| {
            let (user, pass) = creds_or_default(&user, &pass);
            let weak = weak.clone();
            std::thread::spawn(move || {
                let saved: anyhow::Result<(Vec<crate::store::CameraRow>, _)> =
                    save_discovered_blocking(&xaddr, &user, &pass, &token)
                        .map_err(|e| anyhow::anyhow!(e))
                        .and_then(|(uri, kind)| {
                            let store = Store::open()?;
                            let outcome = store.add_camera(&name, &uri, &user, &pass, &kind)?;
                            store.cameras().map(|list| (list, outcome))
                        });
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    match saved {
                        Ok((list, outcome)) => {
                            props::set_camera_entries(&ui, &list);
                            toast_save_outcome(&ui, outcome);
                        }
                        Err(e) => {
                            warn!(error = %e, "save discovered camera failed");
                            ui.invoke_notify(
                                NoticeLevel::Warn as i32,
                                NoticeKind::CameraSaveFailed as i32,
                                slint::SharedString::from(e.to_string()),
                            );
                        }
                    }
                });
            });
        });
    }
}

/// add_camera 合并结果 → toast: 插入/覆盖更新都报已保存, 跳过提示设备已存在
fn toast_save_outcome(ui: &MainWindow, outcome: crate::store::CameraSaveOutcome) {
    let kind = match outcome {
        crate::store::CameraSaveOutcome::Inserted | crate::store::CameraSaveOutcome::Updated => {
            NoticeKind::CameraSaved
        }
        crate::store::CameraSaveOutcome::Skipped => NoticeKind::CameraDuplicate,
    };
    ui.invoke_notify(NoticeLevel::Info as i32, kind as i32, "".into());
}

/// 阻塞版 profile 拉取 (独立线程 + 一次性 runtime)
///
///
///
/// # Arguments
///
/// * `xaddr`: ONVIF 设备服务地址
/// * `user`: 设备用户名
/// * `pass`: 设备密码
///
/// returns: Result<Vec<ipcam_core::VideoProfile>, ipcam_discovery::DiscoveryError>
///
/// # Examples
///
/// ```
///
/// ```
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
/// 否则 get_stream_uri 现取 (凭据此时必然有效, 上一步刚验证过).
/// 返回 (uri, kind) — kind 供 cameras 表同 IP 合并排序用
///
///
///
/// # Arguments
///
/// * `xaddr`: ONVIF 设备服务地址
/// * `user`: 设备用户名
/// * `pass`: 设备密码
/// * `token`: 选中的 profile token
///
/// returns: Result<(String, String), ipcam_discovery::DiscoveryError>
///
/// # Examples
///
/// ```
///
/// ```
fn save_discovered_blocking(
    xaddr: &str,
    user: &str,
    pass: &str,
    token: &str,
) -> Result<(String, String), ipcam_discovery::DiscoveryError> {
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
            Some(p) => {
                let kind = kind_tag(p);
                let uri = match &p.uri {
                    Some(uri) => uri.clone(),
                    None => client.get_stream_uri(token).await?,
                };
                Ok((uri, kind))
            }
            None => Err(ipcam_discovery::DiscoveryError::DeviceMgmt(format!(
                "profile {token} not found on device"
            ))),
        }
    })
}

/// profile → 流类型标签 ("h264"/"h265"/"jpeg"/""): codec 已知直接用;
/// 未知时从 token 猜 (狄耐克设备 token 形如 h264_profile/jpeg_profile)
fn kind_tag(p: &ipcam_core::VideoProfile) -> String {
    use ipcam_core::VideoCodec;
    match p.codec {
        VideoCodec::H264 => return "h264".into(),
        VideoCodec::H265 => return "h265".into(),
        _ => {}
    }
    let t = p.profile_id.to_ascii_lowercase();
    if t.contains("h265") || t.contains("hevc") {
        "h265".into()
    } else if t.contains("h264") {
        "h264".into()
    } else if t.contains("jpeg") || t.contains("jpg") {
        "jpeg".into()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_tag_from_codec_or_token() {
        use ipcam_core::{VideoCodec, VideoProfile};
        let p = |codec, token: &str| VideoProfile {
            profile_id: token.into(),
            uri: None,
            codec,
            width: 0,
            height: 0,
            fps: 0.0,
        };
        // codec 已知直接用
        assert_eq!(kind_tag(&p(VideoCodec::H264, "whatever")), "h264");
        assert_eq!(kind_tag(&p(VideoCodec::H265, "whatever")), "h265");
        // codec 未知从 token 猜 (狄耐克风格)
        assert_eq!(kind_tag(&p(VideoCodec::Unknown, "h264_profile")), "h264");
        assert_eq!(kind_tag(&p(VideoCodec::Unknown, "jpeg_profile")), "jpeg");
        assert_eq!(kind_tag(&p(VideoCodec::Unknown, "profile_1")), "");
    }

    #[test]
    fn creds_or_default_fills_empty() {
        assert_eq!(
            creds_or_default("", ""),
            ("admin".to_string(), "123456".to_string())
        );
        assert_eq!(
            creds_or_default("root", ""),
            ("root".to_string(), "123456".to_string())
        );
        assert_eq!(
            creds_or_default("u", "p"),
            ("u".to_string(), "p".to_string())
        );
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

        // 编码未知时退化到 profile_id, 避免多条 profile 都显示 "Unknown"
        let meta_missing = ipcam_core::VideoProfile {
            profile_id: "h264_profile".into(),
            codec: ipcam_core::VideoCodec::Unknown,
            ..unknown
        };
        assert_eq!(profile_label(&meta_missing), "h264_profile");
    }
}

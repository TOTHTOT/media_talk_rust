//! 天气: Open-Meteo (免 key) 拉当前天气, 结果经 UiEvent::Weather 进
//! 事件漏斗上状态栏. 手动城市 (kv) 优先, 留空用 IP 定位, 定位结果
//! 缓存避免重启重定位. 全程失败静默: 从未成功则天气块隐藏.

use std::sync::mpsc::Receiver;
use std::time::Duration;

use serde::Deserialize;
use tracing::{info, warn};

use crate::app::UiEvent;
use crate::bridge::UiHandle;

/// lucide 天气图标索引, 与 status_bar.slint 的映射一一对应
pub(crate) const ICON_SUN: i32 = 0;
pub(crate) const ICON_CLOUD_SUN: i32 = 1;
pub(crate) const ICON_CLOUD: i32 = 2;
pub(crate) const ICON_CLOUD_FOG: i32 = 3;
pub(crate) const ICON_CLOUD_RAIN: i32 = 4;
pub(crate) const ICON_CLOUD_SNOW: i32 = 5;
pub(crate) const ICON_CLOUD_LIGHTNING: i32 = 6;

pub(crate) const KV_CITY: &str = "weather_city";
/// 定位缓存 "lat,lon,name", 城市变更时由设置回调清空
const KV_GEO: &str = "weather_geo";
const REFRESH: Duration = Duration::from_secs(30 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const BACKOFF_MAX: Duration = Duration::from_secs(240);

/// WMO weathercode → lucide 图标索引
fn wmo_icon(wmo: u8) -> i32 {
    match wmo {
        0 | 1 => ICON_SUN,
        2 => ICON_CLOUD_SUN,
        3 => ICON_CLOUD,
        45 | 48 => ICON_CLOUD_FOG,
        51..=67 => ICON_CLOUD_RAIN,
        71..=86 => ICON_CLOUD_SNOW,
        95..=99 => ICON_CLOUD_LIGHTNING,
        _ => ICON_CLOUD,
    }
}

#[derive(Deserialize)]
struct GeoResp {
    results: Option<Vec<GeoHit>>,
}

#[derive(Deserialize)]
struct GeoHit {
    name: String,
    latitude: f64,
    longitude: f64,
}

#[derive(Deserialize)]
struct IpResp {
    success: bool,
    city: Option<String>,
}

#[derive(Deserialize)]
struct WeatherResp {
    current_weather: CurrentWeather,
}

#[derive(Deserialize)]
struct CurrentWeather {
    temperature: f64,
    weathercode: u8,
}

/// 天气 worker: 启动即拉, 成功后每 30 分钟重拉; 失败 1/2/4 分钟退避;
/// wake 收到信号 (设置页改了城市) 立即重拉. 成功一次投递一次
/// UiEvent::Weather, 失败只 warn 不动状态机
pub(crate) fn spawn(handle: UiHandle, wake: Receiver<()>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut backoff = Duration::from_secs(60);
        loop {
            match refresh_once() {
                Some((wmo, temp_c)) => {
                    info!(wmo, temp_c, "weather refreshed");
                    handle.post(UiEvent::Weather {
                        icon: wmo_icon(wmo),
                        temp_c: temp_c.round() as i32,
                    });
                    backoff = Duration::from_secs(60);
                    // 成功: 等 30 分钟或城市变更唤醒
                    let _ = wake.recv_timeout(REFRESH);
                }
                None => {
                    warn!(delay = ?backoff, "weather refresh failed, backing off");
                    let _ = wake.recv_timeout(backoff);
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            }
        }
    })
}

/// 单次刷新: 定位 (缓存优先) + 拉当前天气, 返回 (weathercode, 摄氏温度).
/// 任何一步失败返回 None
fn refresh_once() -> Option<(u8, f64)> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .ok()?;
        let store = crate::store::Store::open().ok()?;
        let geo = match cached_geo(&store) {
            Some(g) => g,
            None => {
                let g = resolve_geo(&client, &store).await?;
                let (la, lo, name) = &g;
                info!(lat = la, lon = lo, city = %name, "location resolved");
                if let Err(e) = store.kv_set(KV_GEO, &format!("{la},{lo},{name}")) {
                    warn!(err = %e, "cache geo failed");
                }
                g
            }
        };
        fetch_current(&client, geo.0, geo.1).await
    })
}

fn cached_geo(store: &crate::store::Store) -> Option<(f64, f64, String)> {
    let raw = store.kv_get(KV_GEO).ok().flatten()?;
    let mut parts = raw.splitn(3, ',');
    let lat: f64 = parts.next()?.parse().ok()?;
    let lon: f64 = parts.next()?.parse().ok()?;
    let name = parts.next()?.to_string();
    Some((lat, lon, name))
}

/// 定位: 手动城市 (kv) 优先, 留空则 IP 自动定位, 统一过 geocoding 拿经纬度
async fn resolve_geo(
    client: &reqwest::Client,
    store: &crate::store::Store,
) -> Option<(f64, f64, String)> {
    let manual = store.kv_get(KV_CITY).ok().flatten();
    let name = match manual.as_deref().map(str::trim) {
        Some(city) if !city.is_empty() => city.to_string(),
        _ => ip_city(client).await?,
    };
    geocode(client, &name).await
}

/// Open-Meteo geocoding (免 key), 取第一条结果
async fn geocode(client: &reqwest::Client, name: &str) -> Option<(f64, f64, String)> {
    let url = format!(
        "https://geocoding-api.open-meteo.com/v1/search?name={}&count=1&language=zh",
        pct_encode(name)
    );
    let text = client.get(&url).send().await.ok()?.text().await.ok()?;
    let resp: GeoResp = serde_json::from_str(&text).ok()?;
    let hit = resp.results?.into_iter().next()?;
    Some((hit.latitude, hit.longitude, hit.name))
}

/// IP 定位 (ipwho.is, 免 key HTTPS), 只用于未配置城市的首次定位
async fn ip_city(client: &reqwest::Client) -> Option<String> {
    let text = client
        .get("https://ipwho.is/")
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    let resp: IpResp = serde_json::from_str(&text).ok()?;
    if resp.success {
        resp.city.filter(|c| !c.is_empty())
    } else {
        None
    }
}

/// Open-Meteo 当前天气 (免 key, 一次请求只取 current_weather)
async fn fetch_current(client: &reqwest::Client, lat: f64, lon: f64) -> Option<(u8, f64)> {
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat}&longitude={lon}&current_weather=true"
    );
    let text = client.get(&url).send().await.ok()?.text().await.ok()?;
    let resp: WeatherResp = serde_json::from_str(&text).ok()?;
    Some((
        resp.current_weather.weathercode,
        resp.current_weather.temperature,
    ))
}

/// 查询参数百分号编码: 中文城市名等非 ASCII 字节必须编码, 其余原样
fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wmo_icon_maps_all_ranges() {
        assert_eq!(wmo_icon(0), ICON_SUN);
        assert_eq!(wmo_icon(1), ICON_SUN);
        assert_eq!(wmo_icon(2), ICON_CLOUD_SUN);
        assert_eq!(wmo_icon(3), ICON_CLOUD);
        assert_eq!(wmo_icon(45), ICON_CLOUD_FOG);
        assert_eq!(wmo_icon(48), ICON_CLOUD_FOG);
        assert_eq!(wmo_icon(51), ICON_CLOUD_RAIN);
        assert_eq!(wmo_icon(61), ICON_CLOUD_RAIN);
        assert_eq!(wmo_icon(67), ICON_CLOUD_RAIN);
        assert_eq!(wmo_icon(71), ICON_CLOUD_SNOW);
        assert_eq!(wmo_icon(86), ICON_CLOUD_SNOW);
        assert_eq!(wmo_icon(95), ICON_CLOUD_LIGHTNING);
        assert_eq!(wmo_icon(99), ICON_CLOUD_LIGHTNING);
        // 未列出的 code 落回多云
        assert_eq!(wmo_icon(4), ICON_CLOUD);
    }

    #[test]
    fn pct_encode_keeps_ascii_and_escapes_utf8() {
        assert_eq!(pct_encode("Shenzhen"), "Shenzhen");
        assert_eq!(pct_encode("深 圳"), "%E6%B7%B1%20%E5%9C%B3");
    }
}

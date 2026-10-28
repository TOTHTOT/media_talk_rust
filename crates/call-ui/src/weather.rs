//! 天气: Open-Meteo (免 key) 拉当前天气, 结果经 UiEvent::Weather 进
//! 事件漏斗上状态栏. 手动城市 (kv) 优先, 留空用 IP 定位, 定位结果
//! 缓存避免重启重定位. 全程失败静默: 从未成功则天气块隐藏.

use std::sync::mpsc::Receiver;
use std::time::Duration;

use serde::Deserialize;
use tracing::{info, warn};

use crate::app::UiEvent;
use crate::bridge::UiHandle;

/// lucide 天气图标索引, 与 clock_card.slint 的 weather-icons 数组顺序一一
/// 对应 — 改这里必须同步改 slint 数组
pub(crate) const ICON_SUN: i32 = 0;
pub(crate) const ICON_MOON_STAR: i32 = 1;
pub(crate) const ICON_CLOUD_SUN: i32 = 2;
pub(crate) const ICON_CLOUD_MOON: i32 = 3;
pub(crate) const ICON_CLOUD: i32 = 4;
pub(crate) const ICON_CLOUD_DRIZZLE: i32 = 5;
pub(crate) const ICON_CLOUD_RAIN: i32 = 6;
pub(crate) const ICON_CLOUD_RAIN_WIND: i32 = 7;
pub(crate) const ICON_CLOUD_SNOW: i32 = 8;
pub(crate) const ICON_CLOUD_LIGHTNING: i32 = 9;
pub(crate) const ICON_CLOUD_HAIL: i32 = 10;
pub(crate) const ICON_CLOUD_FOG: i32 = 11;
pub(crate) const ICON_HAZE: i32 = 12;
pub(crate) const ICON_WIND: i32 = 13;

pub(crate) const KV_CITY: &str = "weather_city";
/// kv 键: Open-Meteo 定位缓存 "lat,lon,name", 城市/key 变更时由设置回调清空
pub(crate) const KV_GEO: &str = "weather_geo";
const REFRESH: Duration = Duration::from_secs(30 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const BACKOFF_MAX: Duration = Duration::from_secs(240);

/// WMO weathercode → lucide 图标索引, is_day 区分昼夜图标.
/// Open-Meteo 的 code 比和风的粗 (毛毛雨/中雨/大雨不分), 细分图标主要
/// 靠和风路径; 这里能区分的尽量区分 (阵雨/雷暴伴冰雹)
fn wmo_icon(wmo: u8, is_day: bool) -> i32 {
    match wmo {
        0 | 1 => {
            if is_day {
                ICON_SUN
            } else {
                ICON_MOON_STAR
            }
        }
        2 => {
            if is_day {
                ICON_CLOUD_SUN
            } else {
                ICON_CLOUD_MOON
            }
        }
        3 => ICON_CLOUD,
        45 | 48 => ICON_CLOUD_FOG,
        51..=57 => ICON_CLOUD_DRIZZLE,
        61..=67 => ICON_CLOUD_RAIN,
        71..=77 => ICON_CLOUD_SNOW,
        80 => ICON_CLOUD_RAIN,
        81 | 82 => ICON_CLOUD_RAIN_WIND,
        85 | 86 => ICON_CLOUD_SNOW,
        95 => ICON_CLOUD_LIGHTNING,
        96 | 99 => ICON_CLOUD_HAIL,
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
    /// Open-Meteo 是否白天 (1=白天), 用于晴/多云的昼夜图标
    #[serde(default)]
    is_day: u8,
}

// ===== 和风天气 (QWeather, 需 key; 留空则走 Open-Meteo) =====

/// kv 键: 和风 API KEY (API KEY 类型凭据, 只用 key= 参数, 不签 JWT)
pub(crate) const KV_KEY: &str = "weather_key";
/// kv 键: 和风 locationID 缓存 "id,name"
pub(crate) const KV_LOC: &str = "weather_loc";

#[derive(Deserialize)]
struct QGeoResp {
    /// 和风业务码, "200" 才是成功
    code: String,
    location: Option<Vec<QGeoHit>>,
}

#[derive(Deserialize)]
struct QGeoHit {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct QNowResp {
    code: String,
    now: Option<QNow>,
}

#[derive(Deserialize)]
struct QNow {
    temp: String,
    icon: String,
}

/// 和风 icon 编号 → lucide 图标索引. 100-104 基础天气, 150+ 为对应夜版;
/// 3xx 雨 (305 小雨 306 中雨 307+ 大雨/暴雨 313 冻雨 309 毛毛雨),
/// 4xx 雪, 5xx 雾/霾/沙尘
fn qweather_icon(code: i32) -> i32 {
    match code {
        100 => ICON_SUN,
        150 => ICON_MOON_STAR,
        102 => ICON_CLOUD_SUN,
        152 => ICON_CLOUD_MOON,
        101 | 103 => ICON_CLOUD_SUN,
        151 | 153 => ICON_CLOUD_MOON,
        104 => ICON_CLOUD,
        309 | 305 => ICON_CLOUD_DRIZZLE,
        302 | 303 => ICON_CLOUD_LIGHTNING,
        304 => ICON_CLOUD_HAIL,
        306 | 399 => ICON_CLOUD_RAIN,
        313 => ICON_CLOUD_RAIN,
        300 | 301 | 307 | 308 | 310..=312 | 314..=318 => ICON_CLOUD_RAIN_WIND,
        400..=407 => ICON_CLOUD_SNOW,
        500 | 501 | 509 | 510 | 514 | 515 => ICON_CLOUD_FOG,
        502 | 511 | 512 | 513 => ICON_HAZE,
        503 | 504 | 507 | 508 => ICON_WIND,
        _ => ICON_CLOUD,
    }
}

/// 天气 worker: 启动即拉, 成功后每 30 分钟重拉; 失败 1/2/4 分钟退避;
/// wake 收到信号 (设置页改了城市/key) 立即重拉. 成功一次投递一次
/// UiEvent::Weather, 失败只 warn 不动状态机
pub(crate) fn spawn(handle: UiHandle, wake: Receiver<()>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut backoff = Duration::from_secs(60);
        loop {
            match refresh_once() {
                Some((icon, temp_c, city)) => {
                    info!(icon, temp_c, %city, "weather refreshed");
                    handle.post(UiEvent::Weather {
                        icon,
                        temp_c: temp_c.round() as i32,
                        city,
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

/// 单次刷新: 配置了和风 key 走 QWeather, 否则走 Open-Meteo.
/// 返回 (lucide 图标索引, 摄氏温度, 城市名), 任何一步失败返回 None
fn refresh_once() -> Option<(i32, f64, String)> {
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
        let key = store
            .kv_get(KV_KEY)
            .ok()
            .flatten()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty());
        match key {
            Some(key) => qweather_refresh(&client, &store, &key).await,
            None => openmeteo_refresh(&client, &store).await,
        }
    })
}

/// Open-Meteo 路径: 定位 (缓存优先) + 拉当前天气
async fn openmeteo_refresh(
    client: &reqwest::Client,
    store: &crate::store::Store,
) -> Option<(i32, f64, String)> {
    let geo = match cached_geo(store) {
        Some(g) => g,
        None => {
            let g = resolve_geo(client, store).await?;
            let (la, lo, name) = &g;
            info!(lat = la, lon = lo, city = %name, "location resolved");
            if let Err(e) = store.kv_set(KV_GEO, &format!("{la},{lo},{name}")) {
                warn!(err = %e, "cache geo failed");
            }
            g
        }
    };
    let (wmo, temp, is_day) = fetch_current(client, geo.0, geo.1).await?;
    Some((wmo_icon(wmo, is_day), temp, geo.2))
}

/// 和风路径: 城市 → locationID (缓存优先) → 实时天气
async fn qweather_refresh(
    client: &reqwest::Client,
    store: &crate::store::Store,
    key: &str,
) -> Option<(i32, f64, String)> {
    let (loc_id, name) = match cached_loc(store) {
        Some(l) => l,
        None => {
            let l = qweather_locate(client, store, key).await?;
            info!(id = %l.0, city = %l.1, "qweather location resolved");
            if let Err(e) = store.kv_set(KV_LOC, &format!("{},{}", l.0, l.1)) {
                warn!(err = %e, "cache qweather location failed");
            }
            l
        }
    };
    let (icon, temp) = qweather_now(client, key, &loc_id, &name).await?;
    Some((icon, temp, name))
}

/// 和风 locationID 缓存 "id,name"; 空串 (缓存已清空) 视为无缓存
fn cached_loc(store: &crate::store::Store) -> Option<(String, String)> {
    let raw = store.kv_get(KV_LOC).ok().flatten()?;
    if raw.is_empty() {
        return None;
    }
    let mut parts = raw.splitn(2, ',');
    let id = parts.next()?.to_string();
    let name = parts.next()?.to_string();
    Some((id, name))
}

/// 和风城市查询: 手动城市 (kv) 优先, 留空则 IP 自动定位
async fn qweather_locate(
    client: &reqwest::Client,
    store: &crate::store::Store,
    key: &str,
) -> Option<(String, String)> {
    let manual = store.kv_get(KV_CITY).ok().flatten();
    let city = match manual.as_deref().map(str::trim) {
        Some(c) if !c.is_empty() => c.to_string(),
        _ => ip_city(client).await?,
    };
    let url = format!(
        "https://geoapi.qweather.com/v2/city/lookup?location={}&key={}",
        pct_encode(&city),
        pct_encode(key)
    );
    let text = client.get(&url).send().await.ok()?.text().await.ok()?;
    let resp: QGeoResp = serde_json::from_str(&text).ok()?;
    if resp.code != "200" {
        // 401 类鉴权错只打业务码, 绝不打 key
        warn!(code = %resp.code, "qweather geo error (401/402 检查和风 key)");
        return None;
    }
    let hit = resp.location?.into_iter().next()?;
    Some((hit.id, hit.name))
}

/// 和风实时天气: 返回 (lucide 图标索引, 摄氏温度)
async fn qweather_now(
    client: &reqwest::Client,
    key: &str,
    loc_id: &str,
    city: &str,
) -> Option<(i32, f64)> {
    let url = format!(
        "https://devapi.qweather.com/v7/weather/now?location={}&key={}",
        pct_encode(loc_id),
        pct_encode(key)
    );
    info!(?url, "requst weather info");
    let text = client.get(&url).send().await.ok()?.text().await.ok()?;
    let resp: QNowResp = serde_json::from_str(&text).ok()?;
    if resp.code != "200" {
        warn!(code = %resp.code, %city, "qweather now error");
        return None;
    }
    let now = resp.now?;
    let icon: i32 = now.icon.parse().ok()?;
    let temp: f64 = now.temp.parse().ok()?;
    Some((qweather_icon(icon), temp))
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

/// 地理编码: Open-Meteo 优先, 无结果回退 Nominatim (OSM).
/// Open-Meteo 的中文名索引不全 (厦门查不到, 深圳/香港可以),
/// OSM 对中文地名覆盖更好; 两家都免 key
async fn geocode(client: &reqwest::Client, name: &str) -> Option<(f64, f64, String)> {
    match openmeteo_geocode(client, name).await {
        Some(hit) => Some(hit),
        None => nom_geocode(client, name).await,
    }
}

/// Open-Meteo geocoding, 取第一条结果
async fn openmeteo_geocode(client: &reqwest::Client, name: &str) -> Option<(f64, f64, String)> {
    let url = format!(
        "https://geocoding-api.open-meteo.com/v1/search?name={}&count=1&language=zh&timezone=Asia%2FShanghai",
        pct_encode(name)
    );
    let text = client.get(&url).send().await.ok()?.text().await.ok()?;
    let resp: GeoResp = serde_json::from_str(&text).ok()?;
    let hit = resp.results?.into_iter().next()?;
    Some((hit.latitude, hit.longitude, hit.name))
}

#[derive(Deserialize)]
struct NominatimHit {
    lat: String,
    lon: String,
    display_name: String,
}

/// Nominatim (OSM) 地理编码兜底. OSM 使用策略要求带 User-Agent
async fn nom_geocode(client: &reqwest::Client, name: &str) -> Option<(f64, f64, String)> {
    let url = format!(
        "https://nominatim.openstreetmap.org/search?q={}&format=jsonv2&limit=1",
        pct_encode(name)
    );
    let text = client
        .get(&url)
        .header(
            reqwest::header::USER_AGENT,
            concat!("media_talk_rust/", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    let mut hits: Vec<NominatimHit> = serde_json::from_str(&text).ok()?;
    let hit = hits.drain(..).next()?;
    let lat: f64 = hit.lat.parse().ok()?;
    let lon: f64 = hit.lon.parse().ok()?;
    Some((lat, lon, nom_city_name(&hit.display_name)))
}

/// OSM display_name 是逗号分隔的全路径 ("厦门市, 福建省, 中国"),
/// 取首段作为城市名
fn nom_city_name(display: &str) -> String {
    display
        .split(',')
        .next()
        .unwrap_or(display)
        .trim()
        .to_string()
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
async fn fetch_current(client: &reqwest::Client, lat: f64, lon: f64) -> Option<(u8, f64, bool)> {
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat}&longitude={lon}&current_weather=true"
    );
    info!(?url, "requst weather info");
    let text = client.get(&url).send().await.ok()?.text().await.ok()?;
    let resp: WeatherResp = serde_json::from_str(&text).ok()?;
    Some((
        resp.current_weather.weathercode,
        resp.current_weather.temperature,
        resp.current_weather.is_day != 0,
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
        // 晴/少云分昼夜
        assert_eq!(wmo_icon(0, true), ICON_SUN);
        assert_eq!(wmo_icon(0, false), ICON_MOON_STAR);
        assert_eq!(wmo_icon(1, true), ICON_SUN);
        assert_eq!(wmo_icon(1, false), ICON_MOON_STAR);
        // 多云分昼夜
        assert_eq!(wmo_icon(2, true), ICON_CLOUD_SUN);
        assert_eq!(wmo_icon(2, false), ICON_CLOUD_MOON);
        assert_eq!(wmo_icon(3, true), ICON_CLOUD); // 阴
        assert_eq!(wmo_icon(45, true), ICON_CLOUD_FOG);
        assert_eq!(wmo_icon(48, true), ICON_CLOUD_FOG);
        // 毛毛雨/冻毛毛雨
        assert_eq!(wmo_icon(51, true), ICON_CLOUD_DRIZZLE);
        assert_eq!(wmo_icon(57, true), ICON_CLOUD_DRIZZLE);
        // 雨/冻雨
        assert_eq!(wmo_icon(61, true), ICON_CLOUD_RAIN);
        assert_eq!(wmo_icon(67, true), ICON_CLOUD_RAIN);
        // 雪
        assert_eq!(wmo_icon(71, true), ICON_CLOUD_SNOW);
        assert_eq!(wmo_icon(77, true), ICON_CLOUD_SNOW);
        assert_eq!(wmo_icon(86, true), ICON_CLOUD_SNOW);
        // 阵雨: 80 小 → 中雨, 81/82 强 → 大雨
        assert_eq!(wmo_icon(80, true), ICON_CLOUD_RAIN);
        assert_eq!(wmo_icon(81, true), ICON_CLOUD_RAIN_WIND);
        assert_eq!(wmo_icon(82, true), ICON_CLOUD_RAIN_WIND);
        // 雷暴 / 雷暴伴冰雹
        assert_eq!(wmo_icon(95, true), ICON_CLOUD_LIGHTNING);
        assert_eq!(wmo_icon(96, true), ICON_CLOUD_HAIL);
        assert_eq!(wmo_icon(99, true), ICON_CLOUD_HAIL);
        // 未列出的 code 落回多云
        assert_eq!(wmo_icon(4, true), ICON_CLOUD);
    }

    #[test]
    fn pct_encode_keeps_ascii_and_escapes_utf8() {
        assert_eq!(pct_encode("Shenzhen"), "Shenzhen");
        assert_eq!(pct_encode("深 圳"), "%E6%B7%B1%20%E5%9C%B3");
    }

    #[test]
    fn nom_city_name_takes_first_segment() {
        assert_eq!(nom_city_name("厦门市, 福建省, 中国"), "厦门市");
        assert_eq!(nom_city_name("Xiamen"), "Xiamen");
        assert_eq!(nom_city_name(""), "");
    }

    #[test]
    fn qweather_icon_maps_all_ranges() {
        // 晴/少云/多云 昼夜分图
        assert_eq!(qweather_icon(100), ICON_SUN); // 晴
        assert_eq!(qweather_icon(150), ICON_MOON_STAR); // 晴 (夜)
        assert_eq!(qweather_icon(102), ICON_CLOUD_SUN); // 少云
        assert_eq!(qweather_icon(152), ICON_CLOUD_MOON); // 少云 (夜)
        assert_eq!(qweather_icon(103), ICON_CLOUD_SUN); // 晴间多云
        assert_eq!(qweather_icon(153), ICON_CLOUD_MOON); // 晴间多云 (夜)
        assert_eq!(qweather_icon(101), ICON_CLOUD_SUN); // 多云
        assert_eq!(qweather_icon(151), ICON_CLOUD_MOON); // 多云 (夜)
        assert_eq!(qweather_icon(104), ICON_CLOUD); // 阴
        // 雨: 毛毛雨/小雨 → 细雨, 中雨/冻雨 → 中雨, 阵雨/大雨+ → 大雨
        assert_eq!(qweather_icon(309), ICON_CLOUD_DRIZZLE); // 毛毛雨
        assert_eq!(qweather_icon(305), ICON_CLOUD_DRIZZLE); // 小雨
        assert_eq!(qweather_icon(306), ICON_CLOUD_RAIN); // 中雨
        assert_eq!(qweather_icon(313), ICON_CLOUD_RAIN); // 冻雨
        assert_eq!(qweather_icon(300), ICON_CLOUD_RAIN_WIND); // 阵雨
        assert_eq!(qweather_icon(310), ICON_CLOUD_RAIN_WIND); // 暴雨
        assert_eq!(qweather_icon(312), ICON_CLOUD_RAIN_WIND); // 特大暴雨
        // 雷暴 / 冰雹
        assert_eq!(qweather_icon(302), ICON_CLOUD_LIGHTNING); // 雷阵雨
        assert_eq!(qweather_icon(304), ICON_CLOUD_HAIL); // 雷阵雨伴冰雹
        // 雪 (含雨夹雪)
        assert_eq!(qweather_icon(400), ICON_CLOUD_SNOW);
        assert_eq!(qweather_icon(404), ICON_CLOUD_SNOW);
        // 雾 / 霾 / 沙尘
        assert_eq!(qweather_icon(500), ICON_CLOUD_FOG); // 薄雾
        assert_eq!(qweather_icon(501), ICON_CLOUD_FOG); // 雾
        assert_eq!(qweather_icon(502), ICON_HAZE); // 霾
        assert_eq!(qweather_icon(512), ICON_HAZE); // 重度霾
        assert_eq!(qweather_icon(503), ICON_WIND); // 扬沙
        assert_eq!(qweather_icon(507), ICON_WIND); // 沙尘暴
        // 未知编号落回多云
        assert_eq!(qweather_icon(999), ICON_CLOUD);
    }
}

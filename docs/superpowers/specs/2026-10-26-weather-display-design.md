# 天气显示功能设计

日期: 2026-10-26
状态: 已与用户对齐 (图标 + 温度; 手动城市优先, 留空则 IP 自动定位)

## 需求

`call-ui` 主页状态栏时钟旁显示天气: lucide 天气图标 + 当前温度.
位置来源双通道: 设置页手动配置城市 (优先); 留空则启动时 IP 自动定位.

## 数据源

**Open-Meteo** (https://open-meteo.com), 免 key / 免注册, HTTPS:

- 地理编码: `GET https://geocoding-api.open-meteo.com/v1/search?name=<城市>&count=1&language=zh`
  → 取第一条结果的 `latitude/longitude/name`
- 天气: `GET https://api.open-meteo.com/v1/forecast?latitude=..&longitude=..&current_weather=true`
  → `current_weather.temperature` + `current_weather.weathercode` (WMO)
- IP 定位 (仅手动留空时, 启动一次): `GET https://ipwho.is/` → `city`

重试: 失败后 1/2/4 分钟退避; 连续失败保留上次成功值; 从未成功则天气块隐藏, 不弹 toast.

## 架构 (全部走既有事件漏斗)

```text
weather worker 线程 (std::thread + 一次性 tokio runtime, 与 fetch_profiles_blocking 同模式)
    │ 启动 + 每 30min + 城市变更信号
    │ 1. 读 kv city (手动优先), 空则 ipwho.is → geocoding
    │ 2. 缓存 lat/lon/name 到 kv, 免重启重定位
    │ 3. 拉 current_weather → (wmo, temp)
    ▼
UiHandle::post(UiEvent::Weather { icon, temp_c })   ← 唯一上 UI 的通道
    ▼
AppState::on_event → weather: Option<WeatherInfo>
    ▼
props::sync_props → slint 属性 weather-icon / weather-temp
    ▼
status_bar.slint: icon >= 0 时显示 lucide 图标 + "23°"
```

- **wmo_icon()** 纯函数 (单测): 0-1→sun(0), 2→cloud-sun(1), 3→cloud(2),
  45/48→cloud-fog(3), 51-67→cloud-rain(4), 71-86→cloud-snow(5),
  95-99→cloud-lightning(6), 其他→cloud(2)
- 配置持久化: SQLite 新增 `kv(key TEXT PRIMARY KEY, value TEXT)` 表,
  `kv_get("weather_city")` / `kv_set`. Store 是 Rc 不可跨线程 → weather
  线程内自行 `Store::open()` 第二个连接 (rusqlite Connection 线程安全,
  独立连接无竞争)
- 城市变更: general 卡片 `set-city(string)` 回调 → 写 kv → 唤醒 channel,
  worker 立即重定位重拉

## 前端

- `status_bar`: `in property <int> weather-icon: -1; in property <string> weather-temp: ""`,
  `weather-icon >= 0` 时在中区时间旁渲染 `Image(lucide) + "23°"`
- main.slint 透传属性; settings general 卡片加城市 `TextField`
  (占位符提示"留空自动定位", @tr), 失焦/回车时发 `set-city`
- 无天气文字, 无 i18n 文案负担; 新增 po: "城市" / "留空自动定位"

## 错误处理

- 全部失败静默: 从未成功 → 天气块隐藏; 有过值 → 显示旧值
- 网络错误只 warn 日志, 不进 Notice 管线 (天气不是用户操作的结果)

## 测试

- `wmo_icon` 映射全量断言
- `UiEvent::Weather` → AppState 存储 + 状态机无副作用
- store kv get/set 往返

## 不做 (YAGNI)

- 天气预报 / 高低温 (用户选"图标 + 温度")
- 天气文字描述 (图标已表达)
- 多城市 / 单位切换 (固定 °C)

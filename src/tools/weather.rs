// SPDX-License-Identifier: AGPL-3.0-or-later

// src/tools/weather.rs
//! Built-in weather (Tier 2 — network).
//!
//! Current conditions + a multi-day forecast for a place. Defaults to
//! **Open-Meteo** — free, global, no API key, and it ships its own keyless
//! geocoding (name → lat/lon), so "weather in Point Cook" works out of the
//! box with zero setup and no Google billing. An admin can switch
//! `weather.provider` to `openweathermap` (with `weather.api_key`) for an
//! alternative source.
//!
//! The same `get_weather` entry point is reused by the companion daily
//! briefing / check-ins so a forecast line can be woven into them.

use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};

use super::{Tier, Tool, ToolArgs, ToolResult};
use crate::config::WeatherConfig;
use crate::MiraError;

const MAX_DAYS: u64 = 16;

// ── Public report types (reused by the briefing) ───────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct WeatherReport {
    pub location:    String,
    pub units_temp:  String,   // "°C" | "°F"
    pub units_wind:  String,   // "km/h" | "mph"
    pub units_precip: String,  // "mm" | "in"
    pub current:     CurrentConditions,
    pub daily:       Vec<DayForecast>,
    pub provider:    String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CurrentConditions {
    pub temperature: f64,
    pub conditions:  String,
    pub wind:        Option<f64>,
    pub humidity:    Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DayForecast {
    pub date:        String,
    pub high:        f64,
    pub low:         f64,
    pub conditions:  String,
    pub precip_chance: Option<f64>,
}

impl WeatherReport {
    /// One-line summary suitable for a briefing or a chat reply.
    pub fn summary_line(&self) -> String {
        let today = self.daily.first();
        let hilo = today
            .map(|d| format!(", {:.0}{t}/{:.0}{t}", d.high, d.low, t = self.units_temp))
            .unwrap_or_default();
        let rain = today
            .and_then(|d| d.precip_chance)
            .map(|p| format!(", {p:.0}% rain"))
            .unwrap_or_default();
        format!(
            "{}: {:.0}{} {}{}{}",
            self.location, self.current.temperature, self.units_temp,
            self.current.conditions, hilo, rain
        )
    }
}

// ── Units ──────────────────────────────────────────────────────────────────────

struct Units { temp: &'static str, wind: &'static str, precip: &'static str, imperial: bool }
fn units_for(cfg: &WeatherConfig) -> Units {
    if cfg.units.eq_ignore_ascii_case("imperial") {
        Units { temp: "°F", wind: "mph", precip: "in", imperial: true }
    } else {
        Units { temp: "°C", wind: "km/h", precip: "mm", imperial: false }
    }
}

// ── Shared HTTP client ─────────────────────────────────────────────────────────

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(12))
        .user_agent("MIRA-weather/1.0")
        .build()
        .map_err(|e| format!("http client: {e}"))
}

// ── Public entry point (tool + briefing) ───────────────────────────────────────

/// Resolve `location` to coordinates and return current + `days`-day forecast.
/// `days` is clamped to 1..=16. Provider + units come from `cfg`.
pub async fn get_weather(cfg: &WeatherConfig, location: &str, days: u64) -> Result<WeatherReport, String> {
    let days = days.clamp(1, MAX_DAYS);
    let loc = location.trim();
    if loc.is_empty() {
        return Err("no location given".to_string());
    }
    let client = http_client()?;
    match cfg.provider.as_str() {
        "openweathermap" => openweathermap(&client, cfg, loc, days).await,
        _                => open_meteo(&client, cfg, loc, days).await, // default
    }
}

// ── Open-Meteo (default, keyless) ──────────────────────────────────────────────

async fn open_meteo(client: &reqwest::Client, cfg: &WeatherConfig, loc: &str, days: u64) -> Result<WeatherReport, String> {
    let u = units_for(cfg);
    // 1) Geocode (keyless).
    let geo: Value = client
        .get("https://geocoding-api.open-meteo.com/v1/search")
        .query(&[("name", loc), ("count", "1"), ("language", "en"), ("format", "json")])
        .send().await.map_err(|e| format!("geocode: {}", e.without_url()))?
        .json().await.map_err(|e| format!("geocode decode: {e}"))?;
    let first = geo.get("results").and_then(|r| r.get(0))
        .ok_or_else(|| format!("couldn't find a place called '{loc}'"))?;
    let lat = first.get("latitude").and_then(|v| v.as_f64()).ok_or("geocode: no latitude")?;
    let lon = first.get("longitude").and_then(|v| v.as_f64()).ok_or("geocode: no longitude")?;
    let place = {
        let name = first.get("name").and_then(|v| v.as_str()).unwrap_or(loc);
        let admin = first.get("admin1").and_then(|v| v.as_str());
        let country = first.get("country").and_then(|v| v.as_str());
        match (admin, country) {
            (Some(a), Some(c)) => format!("{name}, {a}, {c}"),
            (None, Some(c))    => format!("{name}, {c}"),
            _                  => name.to_string(),
        }
    };

    // 2) Forecast.
    let (t_unit, w_unit, p_unit) = if u.imperial {
        ("fahrenheit", "mph", "inch")
    } else {
        ("celsius", "kmh", "mm")
    };
    let days_s = days.to_string();
    let fc: Value = client
        .get("https://api.open-meteo.com/v1/forecast")
        .query(&[
            ("latitude", lat.to_string().as_str()),
            ("longitude", lon.to_string().as_str()),
            ("current", "temperature_2m,weather_code,wind_speed_10m,relative_humidity_2m"),
            ("daily", "weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max"),
            ("timezone", "auto"),
            ("forecast_days", days_s.as_str()),
            ("temperature_unit", t_unit),
            ("wind_speed_unit", w_unit),
            ("precipitation_unit", p_unit),
        ])
        .send().await.map_err(|e| format!("forecast: {}", e.without_url()))?
        .json().await.map_err(|e| format!("forecast decode: {e}"))?;

    let cur = fc.get("current").ok_or("forecast: missing current")?;
    let current = CurrentConditions {
        temperature: cur.get("temperature_2m").and_then(|v| v.as_f64()).unwrap_or(f64::NAN),
        conditions:  wmo_text(cur.get("weather_code").and_then(|v| v.as_i64()).unwrap_or(-1)).to_string(),
        wind:        cur.get("wind_speed_10m").and_then(|v| v.as_f64()),
        humidity:    cur.get("relative_humidity_2m").and_then(|v| v.as_f64()),
    };

    let daily = fc.get("daily").ok_or("forecast: missing daily")?;
    let dates = daily.get("time").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let highs = daily.get("temperature_2m_max").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let lows  = daily.get("temperature_2m_min").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let codes = daily.get("weather_code").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let pops  = daily.get("precipitation_probability_max").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mut out_days = Vec::new();
    for i in 0..dates.len() {
        out_days.push(DayForecast {
            date:       dates[i].as_str().unwrap_or("").to_string(),
            high:       highs.get(i).and_then(|v| v.as_f64()).unwrap_or(f64::NAN),
            low:        lows.get(i).and_then(|v| v.as_f64()).unwrap_or(f64::NAN),
            conditions: wmo_text(codes.get(i).and_then(|v| v.as_i64()).unwrap_or(-1)).to_string(),
            precip_chance: pops.get(i).and_then(|v| v.as_f64()),
        });
    }

    Ok(WeatherReport {
        location: place, units_temp: u.temp.into(), units_wind: u.wind.into(),
        units_precip: u.precip.into(), current, daily: out_days, provider: "open_meteo".into(),
    })
}

// ── OpenWeatherMap (keyed) ─────────────────────────────────────────────────────

async fn openweathermap(client: &reqwest::Client, cfg: &WeatherConfig, loc: &str, days: u64) -> Result<WeatherReport, String> {
    let key = cfg.api_key.as_deref().filter(|k| !k.trim().is_empty())
        .ok_or("weather.provider=openweathermap but weather.api_key is unset")?;
    let u = units_for(cfg);
    let owm_units = if u.imperial { "imperial" } else { "metric" };

    // Geocode.
    let geo: Value = client.get("https://api.openweathermap.org/geo/1.0/direct")
        .query(&[("q", loc), ("limit", "1"), ("appid", key)])
        .send().await.map_err(|e| format!("geocode: {}", e.without_url()))?
        .json().await.map_err(|e| format!("geocode decode: {e}"))?;
    let first = geo.as_array().and_then(|a| a.first())
        .ok_or_else(|| format!("couldn't find a place called '{loc}'"))?;
    let lat = first.get("lat").and_then(|v| v.as_f64()).ok_or("geocode: no lat")?;
    let lon = first.get("lon").and_then(|v| v.as_f64()).ok_or("geocode: no lon")?;
    let place = {
        let name = first.get("name").and_then(|v| v.as_str()).unwrap_or(loc);
        let country = first.get("country").and_then(|v| v.as_str());
        match country { Some(c) => format!("{name}, {c}"), None => name.to_string() }
    };

    // Current.
    let now: Value = client.get("https://api.openweathermap.org/data/2.5/weather")
        .query(&[("lat", lat.to_string().as_str()), ("lon", lon.to_string().as_str()),
                 ("units", owm_units), ("appid", key)])
        .send().await.map_err(|e| format!("current: {}", e.without_url()))?
        .json().await.map_err(|e| format!("current decode: {e}"))?;
    let current = CurrentConditions {
        temperature: now.pointer("/main/temp").and_then(|v| v.as_f64()).unwrap_or(f64::NAN),
        conditions:  now.pointer("/weather/0/description").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        wind:        now.pointer("/wind/speed").and_then(|v| v.as_f64()),
        humidity:    now.pointer("/main/humidity").and_then(|v| v.as_f64()),
    };

    // Daily from the free 5-day/3-hour forecast: aggregate per calendar date.
    let fc: Value = client.get("https://api.openweathermap.org/data/2.5/forecast")
        .query(&[("lat", lat.to_string().as_str()), ("lon", lon.to_string().as_str()),
                 ("units", owm_units), ("appid", key)])
        .send().await.map_err(|e| format!("forecast: {}", e.without_url()))?
        .json().await.map_err(|e| format!("forecast decode: {e}"))?;
    use std::collections::BTreeMap;
    let mut agg: BTreeMap<String, (f64, f64, String, f64)> = BTreeMap::new(); // date → (hi, lo, cond, pop)
    for slot in fc.get("list").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
        let dt = slot.get("dt_txt").and_then(|v| v.as_str()).unwrap_or("");
        let date = dt.split(' ').next().unwrap_or("").to_string();
        if date.is_empty() { continue; }
        let t = slot.pointer("/main/temp").and_then(|v| v.as_f64()).unwrap_or(f64::NAN);
        let cond = slot.pointer("/weather/0/description").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let pop = slot.get("pop").and_then(|v| v.as_f64()).map(|p| p * 100.0).unwrap_or(0.0);
        let e = agg.entry(date).or_insert((f64::MIN, f64::MAX, cond.clone(), 0.0));
        if t > e.0 { e.0 = t; }
        if t < e.1 { e.1 = t; }
        if pop > e.3 { e.3 = pop; }
    }
    let daily: Vec<DayForecast> = agg.into_iter().take(days as usize).map(|(date, (hi, lo, cond, pop))| {
        DayForecast { date, high: hi, low: lo, conditions: cond, precip_chance: Some(pop) }
    }).collect();

    Ok(WeatherReport {
        location: place, units_temp: u.temp.into(), units_wind: u.wind.into(),
        units_precip: u.precip.into(), current, daily, provider: "openweathermap".into(),
    })
}

// ── WMO weather-code → text (Open-Meteo) ───────────────────────────────────────

fn wmo_text(code: i64) -> &'static str {
    match code {
        0 => "clear sky",
        1 => "mainly clear", 2 => "partly cloudy", 3 => "overcast",
        45 | 48 => "fog",
        51 | 53 | 55 => "drizzle",
        56 | 57 => "freezing drizzle",
        61 | 63 | 65 => "rain",
        66 | 67 => "freezing rain",
        71 | 73 | 75 => "snow",
        77 => "snow grains",
        80 | 81 | 82 => "rain showers",
        85 | 86 => "snow showers",
        95 => "thunderstorm",
        96 | 99 => "thunderstorm with hail",
        _ => "unknown",
    }
}

// ── Tool ───────────────────────────────────────────────────────────────────────

/// Default a missing location from the caller's IANA timezone (injected as
/// `_user_tz`): "Australia/Melbourne" → "Melbourne". Also used by the
/// companion briefing to resolve a location for the weather line.
pub fn location_from_tz(tz: Option<&str>) -> Option<String> {
    let tz = tz?;
    let city = tz.rsplit('/').next()?.replace('_', " ");
    if city.is_empty() { None } else { Some(city) }
}

pub struct WeatherTool {
    cfg: WeatherConfig,
}

impl WeatherTool {
    pub fn new(cfg: WeatherConfig) -> Self { Self { cfg } }
}

#[async_trait]
impl Tool for WeatherTool {
    fn name(&self) -> &str { "weather" }

    fn description(&self) -> &str {
        "Get the current weather and a multi-day forecast for a place. Use \
         this whenever the user asks about weather, temperature, rain, or \
         conditions. If the user doesn't name a location, omit `location` and \
         it defaults to their own timezone's city."
    }

    fn tier(&self) -> Tier { Tier::Network }

    fn args_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "location": {
                    "type": "string",
                    "description": "City / place name (e.g. 'Point Cook' or 'Tokyo, Japan'). Omit to use the user's own location."
                },
                "days": {
                    "type": "integer",
                    "description": "How many days of forecast to return (1-16). Default 3.",
                    "minimum": 1, "maximum": 16
                }
            }
        })
    }

    async fn execute(&self, args: ToolArgs) -> Result<ToolResult, MiraError> {
        let days = args.get("days").and_then(|v| v.as_u64()).unwrap_or(3);
        let explicit = args.get("location").and_then(|v| v.as_str())
            .map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        let location = match explicit {
            Some(l) => l,
            None => match location_from_tz(args.get("_user_tz").and_then(|v| v.as_str())) {
                Some(l) => l,
                None => return Ok(ToolResult::failure(
                    "weather: no location given and the user's location is unknown — ask them which place.")),
            },
        };

        match get_weather(&self.cfg, &location, days).await {
            Ok(report) => {
                let body = json!({
                    "summary": report.summary_line(),
                    "location": report.location,
                    "provider": report.provider,
                    "units": { "temperature": report.units_temp, "wind": report.units_wind, "precip": report.units_precip },
                    "current": report.current,
                    "forecast": report.daily,
                });
                Ok(ToolResult::success(body.to_string()))
            }
            Err(e) => Ok(ToolResult::failure(format!("weather: {e}"))),
        }
    }
}

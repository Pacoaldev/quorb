//! 9Router — pasarela local que enruta hacia 40+ proveedores y ya lleva su
//! propio registro de uso/coste. Quorb lo lee como un proveedor agregador.
//!
//! Endpoints (servidor local, por defecto `http://localhost:20128`):
//!   · `POST /api/auth/login`       → inicia sesión con la contraseña del dashboard
//!   · `GET  /api/usage/stats`      → totales, gasto y desglose por proveedor/modelo
//!   · `GET  /api/usage/providers`  → sub-proveedores vistos (Claude, Kiro, GLM…)
//!   · `GET  /api/usage/chart`      → serie temporal (tokens/coste) para el historial
//!
//! Las rutas `/api/usage/*` están tras el guard del dashboard: exigen la cookie
//! `auth_token` (JWT, 24 h) o, si el usuario desactivó el login, ninguna. Obtenemos
//! la cookie con el mismo login que usa el dashboard y la guardamos en el almacén
//! seguro del sistema; se renueva sola cuando caduca.

use super::{net_error, Who};
use crate::config::NineRouterConfig;
use crate::model::{Daily, Kind, Meter, Spend, Status, UsageReport};
use crate::secrets;
use chrono::Datelike;
use serde_json::Value;
use std::collections::BTreeMap;

/// Error al leer 9Router: hay que volver a iniciar sesión, o es otra cosa.
#[derive(Debug, PartialEq)]
pub enum RouterError {
    Expired,
    Other(String),
}

impl RouterError {
    fn message(self) -> String {
        match self {
            RouterError::Expired => "La sesión de 9Router ha caducado. Vuelve a introducir la contraseña en Ajustes.".into(),
            RouterError::Other(m) => m,
        }
    }
}

/// Inicia sesión en el dashboard local y devuelve la cabecera `Cookie` resultante.
/// `POST /api/auth/login { "password": … }` → `Set-Cookie: auth_token=…`.
pub async fn login(http: &reqwest::Client, base_url: &str, password: &str) -> Result<String, String> {
    if password.trim().is_empty() {
        return Err("Escribe la contraseña del dashboard de 9Router".into());
    }
    let res = http
        .post(format!("{}/api/auth/login", base_url.trim_end_matches('/')))
        .json(&serde_json::json!({ "password": password }))
        .send()
        .await
        .map_err(net_error)?;
    match res.status().as_u16() {
        200..=299 => {}
        401 | 403 => return Err("9Router rechazó la contraseña.".into()),
        429 => return Err("Demasiados intentos en 9Router; espera unos minutos.".into()),
        code => return Err(format!("9Router respondió HTTP {code} al iniciar sesión")),
    }
    let mut jar: BTreeMap<String, String> = BTreeMap::new();
    for v in res.headers().get_all(reqwest::header::SET_COOKIE) {
        if let Some((k, val)) = v.to_str().ok().and_then(super::nan_cloud::parse_set_cookie) {
            if !val.is_empty() {
                jar.insert(k, val);
            }
        }
    }
    let cookie = super::nan_cloud::cookie_header(&jar);
    if cookie.is_empty() {
        return Err("9Router no devolvió una sesión. Comprueba la contraseña.".into());
    }
    Ok(cookie)
}

/// Inicia sesión con la contraseña guardada y guarda la cookie resultante.
/// `None` si no hay contraseña configurada.
pub async fn login_with_saved(http: &reqwest::Client, base_url: &str) -> Result<Option<String>, String> {
    let Some(password) = secrets::get(secrets::NINE_ROUTER_PASSWORD) else {
        return Ok(None);
    };
    let cookie = login(http, base_url, &password).await?;
    secrets::set(secrets::NINE_ROUTER_SESSION, &cookie)?;
    Ok(Some(cookie))
}

/// `GET` con la cookie de sesión; `Expired` si 401/403. Sin cookie (login del
/// dashboard desactivado) se omite la cabecera.
async fn get_json(http: &reqwest::Client, base_url: &str, path: &str, cookie: &str) -> Result<Value, RouterError> {
    let mut req = http
        .get(format!("{}{}", base_url.trim_end_matches('/'), path))
        .header(reqwest::header::ACCEPT, "application/json");
    if !cookie.trim().is_empty() {
        req = req.header(reqwest::header::COOKIE, cookie);
    }
    let res = req.send().await.map_err(|e| RouterError::Other(net_error(e)))?;
    match res.status().as_u16() {
        200 => res.json().await.map_err(|e| RouterError::Other(format!("respuesta no JSON: {e}"))),
        401 | 403 => Err(RouterError::Expired),
        code => Err(RouterError::Other(format!("HTTP {code} en {path}"))),
    }
}

/// Comprueba que el 9Router responde y hay sesión; devuelve el JSON de stats en crudo
/// (para el botón «Probar» de Ajustes). Reintenta con la contraseña si la sesión caducó.
pub async fn probe(http: &reqwest::Client, cfg: &NineRouterConfig) -> Result<String, String> {
    let base = cfg.base_url.trim_end_matches('/');
    // ¿Está vivo? (endpoint público, con CORS abierto)
    http.get(format!("{base}/api/health"))
        .send()
        .await
        .map_err(|e| format!("No encuentro 9Router en {base}: {}", net_error(e)))?;

    let cookie = match secrets::get(secrets::NINE_ROUTER_SESSION) {
        Some(c) => c,
        None => match login_with_saved(http, base).await? {
            Some(c) => c,
            None => return Err("Configura la contraseña del dashboard de 9Router.".into()),
        },
    };
    match get_stats(http, base, &cfg.period, &cookie).await {
        Ok(v) => Ok(serde_json::to_string_pretty(&v).unwrap_or_default()),
        Err(RouterError::Expired) => match login_with_saved(http, base).await? {
            Some(c) => get_stats(http, base, &cfg.period, &c)
                .await
                .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
                .map_err(RouterError::message),
            None => Err("La sesión caducó y no hay contraseña guardada.".into()),
        },
        Err(e) => Err(e.message()),
    }
}

async fn get_stats(http: &reqwest::Client, base: &str, period: &str, cookie: &str) -> Result<Value, RouterError> {
    get_json(http, base, &format!("/api/usage/stats?period={period}"), cookie).await
}

/// Fallo al obtener las stats, ya resuelto el tema de la sesión.
#[derive(Debug, PartialEq)]
enum StatsError {
    /// El dashboard exige sesión y no hay contraseña guardada para renovarla.
    NeedsLogin,
    /// Otro error de red/servidor.
    Other(RouterError),
}

impl StatsError {
    /// Estado del orbe cuando falla la lectura de stats.
    fn status(&self) -> Status {
        match self {
            StatsError::NeedsLogin => Status::NeedsLogin,
            StatsError::Other(e) => status_for(e),
        }
    }
}

/// Lee las stats resolviendo la sesión: usa la cookie actual; si el dashboard no
/// tiene login (sin cookie) o la sesión caducó pero hay contraseña, reintenta tras
/// iniciar sesión. Si el servidor rechaza y no hay credenciales, pide configurarlas.
async fn read_stats(
    http: &reqwest::Client,
    base: &str,
    period: &str,
    cookie: &mut Option<String>,
) -> Result<Value, StatsError> {
    // Primero con la cookie que tengamos (o sin ninguna: login desactivado).
    let first = get_stats(http, base, period, cookie.as_deref().unwrap_or("")).await;
    match first {
        Ok(v) => Ok(v),
        // El servidor exige sesión: si tenemos contraseña, iniciamos y reintentamos.
        Err(RouterError::Expired) => match login_with_saved(http, base).await {
            Ok(Some(c)) => {
                *cookie = Some(c.clone());
                get_stats(http, base, period, &c).await.map_err(StatsError::Other)
            }
            Ok(None) => Err(StatsError::NeedsLogin),
            Err(e) => Err(StatsError::Other(RouterError::Other(e))),
        },
        Err(e) => Err(StatsError::Other(e)),
    }
}

/// Sub-proveedor descubierto en `byProvider`.
pub(crate) struct SubProvider {
    /// Clave tal como la devuelve 9Router (`Kiro`, `claude`). Se usa para volver a
    /// buscar en `byProvider`, que distingue mayúsculas; **no** se normaliza.
    pub key: String,
    /// id de cuenta (`nine_router:<normalizado>`): minúsculas, dígitos y guiones,
    /// para casar con `is_account_id` y ser estable como sufijo.
    pub account_id: String,
    /// Nombre visible («Kiro»).
    pub label: String,
}

/// Sub-proveedores con gasto por encima del umbral, de mayor a menor coste.
/// Se omiten los que no llegan al mínimo (por defecto 0 $, o sea gasto > 0): la
/// tira se llenaría de ceros. Se conserva la clave original para poder buscar en
/// `byProvider` (que distingue mayúsculas: `Kiro` ≠ `kiro`).
fn sub_providers_from(stats: &Value, min_spend: f64) -> Vec<SubProvider> {
    let threshold = if min_spend.is_finite() { min_spend.max(0.0) } else { 0.0 };
    let mut entries: Vec<(String, f64)> = stats
        .get("byProvider")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(id, v)| (id.trim().to_string(), v.get("cost").and_then(Value::as_f64).unwrap_or(0.0))).collect())
        .unwrap_or_default();
    entries.retain(|(key, cost)| {
        // Estrictamente por encima del umbral: con 0 se exige algo de gasto.
        let account_id = format!("nine_router:{}", normalize_sub_id(key));
        *cost > threshold && !key.is_empty() && crate::config::is_account_id(&account_id)
    });
    entries.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    entries
        .into_iter()
        .map(|(key, _)| {
            let account_id = format!("nine_router:{}", normalize_sub_id(&key));
            let label = if key.is_empty() { account_id.clone() } else { key.clone() };
            SubProvider { key, account_id, label }
        })
        .collect()
}

/// Sufijo de cuenta para un sub-proveedor: minúsculas, y todo lo que no sea
/// `[a-z0-9-]` pasa a guion (p. ej. nodos personalizados con espacios).
fn normalize_sub_id(key: &str) -> String {
    key.trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' { c } else { '-' })
        .collect()
}

/// Informe del proveedor agregador (o de uno de sus sub-proveedores si `who` es extra).
pub async fn fetch(cfg: &NineRouterConfig, who: Who<'_>, http: &reqwest::Client) -> UsageReport {
    // El orbe agregado + sus sub-proveedores comparten una única lectura de stats;
    // aquí solo devolvemos el report que pida `who` (la cuenta extra es del fetch_all).
    match who.label {
        None => fetch_all(cfg, http).await.into_iter().next().unwrap_or_else(|| {
            Who::main("nine_router").report("9Router", Kind::Api).fail(Status::Error, "Sin datos de 9Router")
        }),
        Some(_) => fetch_all(cfg, http).await.into_iter().find(|r| r.id == who.id).unwrap_or_else(|| {
            who.report("9Router", Kind::Api)
                .fail(Status::Warning, "9Router aún no ha registrado uso de este proveedor en el periodo.")
        }),
    }
}

/// Lee las stats una sola vez y devuelve el orbe agregado seguido de un informe
/// por cada sub-proveedor visto (`nine_router:<id>`). Es la entrada que usa
/// `fetch_all` para no pedir las stats una vez por cuenta.
pub async fn fetch_all(cfg: &NineRouterConfig, http: &reqwest::Client) -> Vec<UsageReport> {
    let main_who = Who::main("nine_router");
    let main_report = main_who.report("9Router", Kind::Api);
    let base = cfg.base_url.trim_end_matches('/');

    // Sesión: guardada, o login con la contraseña configurada. Si no hay ninguna,
    // probamos a leer sin cookie: el login del dashboard puede estar desactivado.
    let mut cookie = match secrets::get(secrets::NINE_ROUTER_SESSION) {
        Some(c) => Some(c),
        None => match login_with_saved(http, base).await {
            Ok(c) => c,
            Err(e) => return vec![main_report.fail(Status::Error, e)],
        },
    };

    let stats = match read_stats(http, base, &cfg.period, &mut cookie).await {
        Ok(v) => v,
        Err(e) => {
            let status = e.status();
            let message = match e {
                StatsError::NeedsLogin => "La sesión de 9Router caducó y no hay contraseña guardada.".to_string(),
                StatsError::Other(err) => err.message(),
            };
            return vec![main_report.fail(status, message)];
        }
    };

    // Serie opcional para el historial; si falla, seguimos sin ella.
    let chart = get_json(http, base, &format!("/api/usage/chart?period={}", cfg.period), cookie.as_deref().unwrap_or(""))
        .await
        .ok();

    let mut out = vec![build_main(cfg, &main_report, &stats, chart.as_ref())];
    if cfg.show_sub_providers {
        for sub in sub_providers_from(&stats, cfg.min_spend_usd) {
            let account = crate::config::Account {
                id: sub.account_id.clone(),
                provider: "nine_router".into(),
                label: sub.label.clone(),
                enabled: true,
                ..Default::default()
            };
            out.push(build_sub(&Who::extra(&account).report("9Router", Kind::Api), &stats, &sub.key, chart.as_ref()));
        }
    }
    out
}

fn status_for(e: &RouterError) -> Status {
    match e {
        RouterError::Expired => Status::NeedsLogin,
        RouterError::Other(_) => Status::Error,
    }
}

/// Sub-proveedor concreto de 9Router (cuenta adicional `nine_router:<id>`).
fn build_sub(report: &UsageReport, stats: &Value, provider: &str, chart: Option<&Value>) -> UsageReport {
    let mut report = report.clone();

    let Some(entry) = stats.get("byProvider").and_then(|b| b.get(provider)).cloned() else {
        return report.fail(
            Status::Warning,
            format!("9Router aún no ha registrado uso de «{provider}» en este periodo."),
        );
    };
    let p = parse_provider(&entry);
    report.plan = Some("Vía 9Router".into());
    report.meters = p.meters;
    report.spend = p.spend;
    // La serie `/api/usage/chart` es agregada (todos los proveedores): no se puede
    // atribuir a un sub-proveedor sin un desglose por proveedor. Antes de exponer
    // datos de otros, se omite; si la chart trae un campo por proveedor, se usa.
    report.daily = chart.map(|c| parse_daily_for(c, Some(provider))).unwrap_or_default();
    report.message = p.message;
    report
}

/// Orbe agregador: totales del periodo + sub-proveedores activos.
fn build_main(cfg: &NineRouterConfig, report: &UsageReport, stats: &Value, chart: Option<&Value>) -> UsageReport {
    let mut report = report.clone();
    let totals = parse_totals(stats);

    let mut meters = vec![];
    if let Some(budget) = cfg.monthly_budget_usd.filter(|b| *b > 0.0) {
        meters.push(Meter {
            label: "Gasto (periodo)".into(),
            percent: Some(totals.cost / budget * 100.0),
            used: Some(totals.cost),
            limit: Some(budget),
            unit: Some("USD".into()),
            resets_at: None,
            window_seconds: None,
        });
    }
    meters.push(Meter {
        label: "Tokens".into(),
        percent: None,
        used: Some(totals.tokens as f64),
        limit: None,
        unit: Some("tokens".into()),
        resets_at: None,
        window_seconds: None,
    });

    let providers = stats.get("byProvider").and_then(Value::as_object).map(|m| m.len()).unwrap_or(0);
    report.plan = Some(format!("{providers} proveedor{}", if providers == 1 { "" } else { "es" }));
    report.meters = meters;
    report.spend = Some(Spend {
        amount: totals.cost,
        currency: "USD".into(),
        period_label: period_label(&cfg.period),
        budget: cfg.monthly_budget_usd,
    });
    report.daily = chart.map(parse_daily).unwrap_or_default();
    report.message = Some(format!(
        "{} peticiones · {} tokens · {:.2} $",
        totals.requests, totals.tokens, totals.cost
    ));
    report
}

struct Totals {
    requests: u64,
    tokens: u64,
    cost: f64,
}

fn parse_totals(v: &Value) -> Totals {
    let num = |k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    Totals {
        requests: num("totalRequests") as u64,
        tokens: (num("totalPromptTokens") + num("totalCompletionTokens")) as u64,
        cost: num("totalCost"),
    }
}

struct Parsed {
    meters: Vec<Meter>,
    spend: Option<Spend>,
    message: Option<String>,
}

/// Desglose por proveedor de `byProvider` en un medidor + gasto.
fn parse_provider(entry: &Value) -> Parsed {
    let num = |k: &str| entry.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let requests = num("requests") as u64;
    let tokens = (num("promptTokens") + num("completionTokens")) as u64;
    let cost = num("cost");

    let meters = vec![Meter {
        label: "Tokens (periodo)".into(),
        percent: None,
        used: Some(tokens as f64),
        limit: None,
        unit: Some("tokens".into()),
        resets_at: None,
        window_seconds: None,
    }];
    let spend = Some(Spend {
        amount: cost,
        currency: "USD".into(),
        period_label: "Periodo".into(),
        budget: None,
    });
    Parsed {
        meters,
        spend,
        message: Some(format!("{requests} peticiones · {:.2} $", cost)),
    }
}

/// Serie `[{label,tokens,cost,requests}]` de la chart → `Daily` (fecha + tokens).
/// El `label` de 9Router es una fecha u hora legible («Oct 5», «14:00»); el
/// frontend espera `YYYY-MM-DD` (le añade `T00:00:00`), así que se normaliza y se
/// agrupa por día sumando tokens. Lo que no se pueda interpretar se descarta.
fn parse_daily(chart: &Value) -> Vec<Daily> {
    parse_daily_for(chart, None)
}

/// Igual que `parse_daily`, pero para un sub-proveedor: si la chart trae un
/// desglose por proveedor, se usa el del indicado; si no, se devuelve vacío en
/// vez de exponer la serie agregada (que incluiría a los demás).
fn parse_daily_for(chart: &Value, provider: Option<&str>) -> Vec<Daily> {
    let mut by_day: BTreeMap<String, f64> = BTreeMap::new();
    for e in chart.as_array().into_iter().flatten() {
        let Some(label) = e.get("label").and_then(Value::as_str) else { continue };
        let Some(date) = normalize_day(label) else { continue };
        let value = match provider {
            None => e.get("tokens").and_then(Value::as_f64).unwrap_or(0.0),
            Some(p) => match e.get("provider").and_then(Value::as_str) {
                Some(name) if name == p => e.get("tokens").and_then(Value::as_f64).unwrap_or(0.0),
                _ => continue,
            },
        };
        *by_day.entry(date).or_insert(0.0) += value;
    }
    by_day.into_iter().map(|(date, value)| Daily { date, value }).collect()
}

/// Normaliza la etiqueta de la chart a `YYYY-MM-DD`. Acepta ISO (`2026-10-05`),
/// `YYYY-MM-DD HH:MM`, fechas ya completas con hora, y etiquetas cortas de mes y
/// día (`Oct 5`, `5 Oct`) u horas (`14:00`) sin contexto de año.
fn normalize_day(label: &str) -> Option<String> {
    let label = label.trim();
    if label.is_empty() {
        return None;
    }
    // Ya viene como fecha ISO completa (posible `T` u hora).
    if let Some(date) = label.get(..10).filter(|s| is_iso_date(s)) {
        return Some((*date).to_string());
    }
    // `YYYY-MM-DD HH:MM` sin T.
    if label.len() >= 10 && is_iso_date(&label[..10]) {
        return Some(label[..10].to_string());
    }
    // Etiqueta corta de mes y día («Oct 5», «5 Oct»), con el año en curso. El
    // nombre del mes se resuelve a mano para no depender del locale de chrono.
    let now = chrono::Utc::now();
    let parts: Vec<&str> = label.split_whitespace().collect();
    if let [a, b] = parts[..] {
        let (month_name, day_str) = if month_number(a).is_some() { (a, b) } else { (b, a) };
        if let (Some(month), Ok(day)) = (month_number(month_name), day_str.parse::<u32>()) {
            return chrono::NaiveDate::from_ymd_opt(now.year(), month, day)
                .map(|d| d.format("%Y-%m-%d").to_string());
        }
    }
    // Solo la hora («14:00»): se atribuye al día de hoy.
    if chrono::NaiveTime::parse_from_str(label, "%H:%M").is_ok() {
        return Some(now.date_naive().format("%Y-%m-%d").to_string());
    }
    None
}

/// ¿Es `s` exactamente `YYYY-MM-DD`?
fn is_iso_date(s: &str) -> bool {
    s.len() == 10
        && s.as_bytes()[4] == b'-'
        && s.as_bytes()[7] == b'-'
        && s.chars().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// Número de mes (1–12) a partir de un nombre en inglés, corto o completo
/// (`oct`/`october`). Independiente del locale.
fn month_number(name: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    const FULL: [&str; 12] = [
        "january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november", "december",
    ];
    let n = name.trim().to_ascii_lowercase();
    MONTHS.iter().position(|m| *m == n).or_else(|| FULL.iter().position(|m| *m == n)).map(|i| i as u32 + 1)
}

fn period_label(period: &str) -> String {
    match period {
        "today" => "Hoy",
        "24h" => "Últimas 24 h",
        "7d" => "Últimos 7 días",
        "30d" => "Últimos 30 días",
        "60d" => "Últimos 60 días",
        _ => "Total",
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn totals_are_summed() {
        let v = json!({"totalRequests": 1662, "totalPromptTokens": 40_000_000u64,
            "totalCompletionTokens": 7_000_000u64, "totalCost": 290.5});
        let t = parse_totals(&v);
        assert_eq!(t.requests, 1662);
        assert_eq!(t.tokens, 47_000_000);
        assert!((t.cost - 290.5).abs() < 1e-6);
    }

    #[test]
    fn provider_breakdown_maps_to_meter_and_spend() {
        let entry = json!({"requests": 100, "promptTokens": 1000, "completionTokens": 500, "cost": 1.25});
        let p = parse_provider(&entry);
        assert_eq!(p.meters[0].used, Some(1500.0));
        assert_eq!(p.meters[0].unit.as_deref(), Some("tokens"));
        assert_eq!(p.spend.as_ref().map(|s| s.amount), Some(1.25));
        assert_eq!(p.message.as_deref(), Some("100 peticiones · 1.25 $"));
    }

    #[test]
    fn chart_becomes_daily_series_with_normalized_dates() {
        // La chart trae etiquetas legibles («Oct 5»), horas y fechas ISO. El
        // frontend espera YYYY-MM-DD y añade T00:00:00: hay que normalizar y
        // agrupar por día (regresión del punto 5 del feedback).
        let year = chrono::Utc::now().year();
        let chart = json!([
            {"label": "Oct 5", "tokens": 120},
            {"label": "14:00", "tokens": 30},
            {"label": format!("{year}-10-05"), "tokens": 5},
            {"label": "vaya", "tokens": 999}
        ]);
        let d = parse_daily(&chart);
        let oct5 = format!("{year}-10-05");
        // Tres muestras caen en dos días (Oct 5 y hoy); «vaya» se descarta.
        assert!(d.iter().all(|x| x.date.len() == 10 && x.date.as_bytes()[4] == b'-'));
        let total: f64 = d.iter().map(|x| x.value).sum();
        assert!((total - 155.0).abs() < 1e-6, "total inesperado: {total}");
        // Las dos etiquetas del 5 de octubre se agrupan en una sola entrada.
        let oct5_total: f64 = d.iter().filter(|x| x.date == oct5).map(|x| x.value).sum();
        assert!((oct5_total - 125.0).abs() < 1e-6, "oct5 inesperado: {oct5_total}");
    }

    #[test]
    fn sub_series_is_empty_without_per_provider_breakdown() {
        // La chart es agregada: un sub-proveedor no debe mostrar tokens de otros.
        let chart = json!([{"label": "Oct 5", "tokens": 120}]);
        assert!(parse_daily_for(&chart, Some("claude")).is_empty());
    }

    #[test]
    fn sub_series_uses_per_provider_field_when_present() {
        let chart = json!([
            {"label": "Oct 5", "tokens": 120, "provider": "claude"},
            {"label": "Oct 5", "tokens": 80, "provider": "kiro"}
        ]);
        let d = parse_daily_for(&chart, Some("claude"));
        assert_eq!(d.len(), 1);
        assert!((d[0].value - 120.0).abs() < 1e-6);
    }

    #[test]
    fn stats_error_maps_to_status() {
        // Regresión del punto 3: sin credenciales y con el servidor exigiendo
        // sesión, el orbe pide login (no «falta configurar»); un fallo de red es Error.
        assert_eq!(StatsError::NeedsLogin.status(), Status::NeedsLogin);
        assert_eq!(StatsError::Other(RouterError::Other("x".into())).status(), Status::Error);
    }

    #[test]
    fn periods_have_labels() {
        assert_eq!(period_label("7d"), "Últimos 7 días");
        assert_eq!(period_label("all"), "Total");
    }

    #[test]
    fn main_meter_uses_budget_when_set() {
        let cfg = NineRouterConfig { monthly_budget_usd: Some(100.0), ..Default::default() };
        let stats = json!({"totalRequests": 10, "totalPromptTokens": 400, "totalCompletionTokens": 100, "totalCost": 50.0,
            "byProvider": {"claude": {}, "kiro": {}}});
        let r = build_main(&cfg, &UsageReport::new("nine_router", "9Router", Kind::Api), &stats, None);
        assert_eq!(r.meters[0].label, "Gasto (periodo)");
        assert_eq!(r.meters[0].percent, Some(50.0));
        assert_eq!(r.plan.as_deref(), Some("2 proveedores"));
    }

    #[test]
    fn parse_set_cookie_reused_from_nan_cloud() {
        // La cookie de 9Router usa el mismo formato que la de NaN.
        assert_eq!(
            super::super::nan_cloud::parse_set_cookie("auth_token=abc.def; Path=/; HttpOnly"),
            Some(("auth_token".into(), "abc.def".into()))
        );
    }

    fn account_ids(subs: &[SubProvider]) -> Vec<String> {
        subs.iter().map(|s| s.account_id.clone()).collect()
    }

    #[test]
    fn discovered_providers_are_valid_account_ids() {
        let stats = json!({"byProvider": {
            "claude": {"cost": 1.5},
            "Kiro": {"cost": 0.5},
            "zz-free": {"cost": 0.0},
            "bad id!": {"cost": 9.0},
            "": {"cost": 9.0}
        }});
        // Solo los que gastan, con id de cuenta saneado (minúsculas, `-`) y
        // ordenados por coste. `bad id!` → `bad-id-`; la clave vacía se omite.
        let subs = sub_providers_from(&stats, 0.0);
        assert_eq!(
            account_ids(&subs),
            vec!["nine_router:bad-id-".to_string(), "nine_router:claude".to_string(), "nine_router:kiro".to_string()]
        );
    }

    #[test]
    fn discovered_provider_keeps_original_key_for_lookup() {
        // Regresión del punto 6: la clave original (`Kiro`) debe conservarse para
        // buscar en `byProvider` (distingue mayúsculas); el id va en minúsculas.
        let stats = json!({"byProvider": {
            "Kiro": {"cost": 0.5, "requests": 3},
            "nodO Personal": {"cost": 1.0}
        }});
        let subs = sub_providers_from(&stats, 0.0);
        let kiro = subs.iter().find(|s| s.account_id == "nine_router:kiro").expect("kiro descubierto");
        assert_eq!(kiro.key, "Kiro");
        assert_eq!(kiro.label, "Kiro");
        // `build_sub` con la clave original encuentra el desglose (no cae en Warning).
        let base = UsageReport::new("nine_router:kiro", "9Router · Kiro", Kind::Api);
        let r = build_sub(&base, &stats, &kiro.key, None);
        assert_eq!(r.status, Status::Ok);
        assert_eq!(r.spend.as_ref().map(|s| s.amount), Some(0.5));
    }

    #[test]
    fn discovered_provider_sanitizes_account_id_but_keeps_key() {
        let stats = json!({"byProvider": {"nodO Personal": {"cost": 1.0}}});
        let subs = sub_providers_from(&stats, 0.0);
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].account_id, "nine_router:nodo-personal");
        assert_eq!(subs[0].key, "nodO Personal");
    }

    #[test]
    fn providers_without_spend_are_skipped() {
        let stats = json!({"byProvider": {"free-a": {"cost": 0}, "free-b": {"requests": 100}}});
        assert!(sub_providers_from(&stats, 0.0).is_empty());
    }

    #[test]
    fn min_spend_filters_low_cost_providers() {
        let stats = json!({"byProvider": {
            "caro": {"cost": 10.0},
            "barato": {"cost": 0.5},
            "gratis": {"cost": 0.0}
        }});
        assert_eq!(account_ids(&sub_providers_from(&stats, 1.0)), vec!["nine_router:caro".to_string()]);
        assert_eq!(
            account_ids(&sub_providers_from(&stats, 0.0)),
            vec!["nine_router:caro".to_string(), "nine_router:barato".to_string()]
        );
    }
}

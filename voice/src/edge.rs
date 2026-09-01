//! Client del servei "Read Aloud" de Microsoft Edge (el mateix que fa servir
//! `msedge-tts`). Protocol venedat d'aqui (port de `rany2/edge-tts`, MIT):
//!
//!   1. Token `Sec-MS-GEC` (hash SHA-256 del Windows-file-time truncat als
//!      5 minuts + el `TRUSTED_CLIENT_TOKEN`).
//!   2. WebSocket WSS a `speech.platform.bing.com/.../edge/v1` amb les
//!      capçaleres del plugin Edge.
//!   3. `Path:speech.config` (output format) + `Path:ssml` (la veu).
//!   4. Frames binaris `Path:audio` -> concatena el MP3 final.
//!
//! Sortida: MP3 `audio-24khz-48kbitrate-mono-mp3` (el mateix format que usa
//! el `voice-convert` Node).

use crate::VoiceError;
use futures_util::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Constants del protocol (vegeu rany2/edge-tts: constants.py + drm.py)
// ---------------------------------------------------------------------------

const WSS_BASE: &str =
    "wss://speech.platform.bing.com/consumer/speech/synthesize/readaloud/edge/v1";
/// Token de client trust (no secret; es el que envia el plugin d'Edge).
const TRUSTED_CLIENT_TOKEN: &str = "6A5AA1D4EAFF4E9FB37E23D68491D6F4";
/// Version de Chromium; Microsoft la rota de tant en tant. Si el servei torna
/// 403, puja aquesta constant (i SEC_MS_GEC_VERSION) a la darrera versio
/// d'Edge (https://learn.microsoft.com/en-us/deployedge/microsoft-edge-relnotes).
const CHROMIUM_FULL_VERSION: &str = "143.0.3650.75";
const SEC_MS_GEC_VERSION: &str = "1-143.0.3650.75";
const OUTPUT_FORMAT: &str = "audio-24khz-48kbitrate-mono-mp3";
/// Marca dels frames binaris que porten veu.
const PATH_AUDIO: &[u8] = b"Path:audio";
/// Epoch de Windows (1601-01-01 a 1970-01-01), en segons.
const WIN_EPOCH_S: i128 = 11_644_473_600;

fn user_agent() -> String {
    let major = CHROMIUM_FULL_VERSION
        .split('.')
        .next()
        .unwrap_or("143");
    format!(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
         (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36 Edg/{major}.0.0.0"
    )
}

// ---------------------------------------------------------------------------
// DRM: token Sec-MS-GEC
// ---------------------------------------------------------------------------

/// Genera el token `Sec-MS-GEC`. Falseja: ticks = (unix + WIN_EPOCH) truncat
/// als 5 minuts, x10^7 (100-nanoseconds), i hash SHA-256 de
/// `"{ticks:.0}{TRUSTED_CLIENT_TOKEN}"` en hexa majuscules.
fn generate_sec_ms_gec() -> String {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i128)
        .unwrap_or(0);
    let mut ticks = unix + WIN_EPOCH_S;
    ticks -= ticks % 300; // arrodonim a baix als 5 minuts
    ticks *= 10_000_000;

    let str_to_hash = format!("{ticks}{TRUSTED_CLIENT_TOKEN}");
    let hash = Sha256::digest(str_to_hash.as_bytes());
    let mut out = String::with_capacity(64);
    for b in hash {
        out.push_str(&format!("{b:02X}"));
    }
    out
}

// ---------------------------------------------------------------------------
// Preparacio del text
// ---------------------------------------------------------------------------

/// El servei no suporta alguns caracters de control (0-8, 11-12, 14-31).
fn remove_incompatible_characters(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let c = ch as u32;
        if (0..=8).contains(&c) || (11..=12).contains(&c) || (14..=31).contains(&c) {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Data JavaScript-style (per a `X-Timestamp`).
fn date_to_string() -> String {
    // "%a %b %d %Y %H:%M:%S GMT+0000 (Coordinated Universal Time)"
    let now = chrono_now();
    format!(
        "{} {} {:02} {} {:02}:{:02}:{:02} GMT+0000 (Coordinated Universal Time)",
        weekday_abbr(now.0),
        month_abbr(now.1),
        now.2,
        now.3,
        now.4,
        now.5,
        now.6
    )
}

/// (weekday_idx, month_idx, day, year, hour, min, sec) en UTC, sense deps.
fn chrono_now() -> (u32, u32, u32, i32, u32, u32, u32) {
    // Algoritme de dias de la setmana de Sakamoto; any es comptat desde el
    // 2000 enlloc del 1900 (January 1, 2000 = dissabte).
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let day_secs = secs.rem_euclid(86_400);
    let hour = (day_secs / 3600) as u32;
    let min = ((day_secs % 3600) / 60) as u32;
    let sec = (day_secs % 60) as u32;

    let days_civil = days + 719_468; // from 1970-01-01
    let era = days_civil.div_euclid(146_097);
    let doe = days_civil.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };

    // Weekday: 1970-01-01 era dijous (4). 2000-01-01 era dissabte (6).
    let wd = ((days + 4).rem_euclid(7)) as u32; // 0=Sunday
    (wd, m as u32, d as u32, year as i32, hour, min, sec)
}

fn weekday_abbr(wd: u32) -> &'static str {
    match wd {
        0 => "Sun",
        1 => "Mon",
        2 => "Tue",
        3 => "Wed",
        4 => "Thu",
        5 => "Fri",
        _ => "Sat",
    }
}

fn month_abbr(m: u32) -> &'static str {
    match m {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        _ => "Dec",
    }
}

// ---------------------------------------------------------------------------
// TLS (rustls ring provider) — cal instal·lar-lo un cop abans del primer WSS.
// ---------------------------------------------------------------------------

static TLS_START: OnceLock<()> = OnceLock::new();

/// Instal·la el crypto provider de rustls (ring), idempotent. Cal cridar-ho
/// abans de qualsevol `synthesize`. Es crida sol des de `synthesize`.
fn ensure_tls() {
    TLS_START.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

// ---------------------------------------------------------------------------
// Sintesi
// ---------------------------------------------------------------------------

pub struct Voice {
    pub voice: String,
    pub lang: String,
    /// prosody rate (p.ex. "+0%"). El DSP fa el canvi de frequencia; aqui es
    /// deixa neutre per defecte.
    pub rate: String,
}

/// Sintetitza `text` amb la veu indicada i torna el MP3 cru (24 kHz mono).
pub async fn synthesize(
    text: &str,
    voice: &str,
    _lang: &str,
    rate: &str,
) -> Result<Vec<u8>, VoiceError> {
    ensure_tls();

    let cleaned = remove_incompatible_characters(text);
    let escaped = xml_escape(&cleaned);
    let conn_id = Uuid::new_v4().simple().to_string();
    let muid = Uuid::new_v4().simple().to_string();
    let gec = generate_sec_ms_gec();

    let url = format!(
        "{WSS_BASE}?TrustedClientToken={TRUSTED_CLIENT_TOKEN}&Sec-MS-GEC={gec}\
         &Sec-MS-GEC-Version={SEC_MS_GEC_VERSION}&ConnectionId={conn_id}"
    );

    let mut request = url
        .into_client_request()
        .map_err(|e| VoiceError::Edge(format!("request: {e}")))?;
    let headers = request.headers_mut();
    headers.insert("Pragma", HeaderValue::from_static("no-cache"));
    headers.insert("Cache-Control", HeaderValue::from_static("no-cache"));
    headers.insert(
        "Origin",
        HeaderValue::from_static("chrome-extension://jdiccldimpdaibmpdkjnbmckianbfold"),
    );
    headers.insert("User-Agent", HeaderValue::from_str(&user_agent()).expect("ua"));
    headers.insert("Accept-Encoding", HeaderValue::from_static("gzip, deflate, br, zstd"));
    headers.insert("Accept-Language", HeaderValue::from_static("en-US,en;q=0.9"));
    headers.insert("Cookie", HeaderValue::from_str(&format!("muid={muid};")).expect("cookie"));

    let (mut ws, _resp) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| VoiceError::Edge(format!("connect: {e}")))?;

    // 1) Configuracio: format de sortida MP3 24 kHz mono.
    let speech_config = format!(
        "X-Timestamp:{}\r\n\
         Content-Type:application/json; charset=utf-8\r\n\
         Path:speech.config\r\n\r\n\
         {{\"context\":{{\"synthesis\":{{\"audio\":{{\"metadataoptions\":{{\
         \"sentenceBoundaryEnabled\":\"false\",\"wordBoundaryEnabled\":\"false\"}},\
         \"outputFormat\":\"{OUTPUT_FORMAT}\"}}}}}}}}\r\n",
        date_to_string()
    );

    // 2) SSML amb la veu i el text.
    let request_id = Uuid::new_v4().simple().to_string();
    let ssml = format!(
        "<speak version='1.0' xmlns='http://www.w3.org/2001/10/synthesis' xml:lang='en-US'>\
         <voice name='{voice}'><prosody pitch='+0Hz' rate='{rate}' volume='+0%'>\
         {escaped}</prosody></voice></speak>"
    );
    // Nota: el "Z" final de X-Timestamp es un bug de Microsoft; el mantindrem com el port.
    let ssml_msg = format!(
        "X-RequestId:{request_id}\r\n\
         Content-Type:application/ssml+xml\r\n\
         X-Timestamp:{}Z\r\n\
         Path:ssml\r\n\r\n\
         {ssml}",
        date_to_string()
    );

    ws.send(Message::Text(speech_config))
        .await
        .map_err(|e| VoiceError::Edge(format!("speech.config: {e}")))?;
    ws.send(Message::Text(ssml_msg))
        .await
        .map_err(|e| VoiceError::Edge(format!("ssml: {e}")))?;

    // 3) Recollim frames d'audio.
    let mut audio: Vec<u8> = Vec::new();
    let mut audio_received = false;

    while let Some(msg) = ws.next().await {
        let msg = msg.map_err(|e| VoiceError::Edge(format!("recv: {e}")))?;
        match msg {
            Message::Text(txt) => {
                // Missatges de text: config, metadades i marques de torn. Només
                // ens interessa `Path:turn.end` per tancar la sintesi.
                if txt.contains("Path:turn.end") {
                    break;
                }
            }
            Message::Binary(bin) => {
                // Format verificat contra Rany2/comm i el crate kothok-edge-tts:
                //   [u16 BE header_len][bloc de headers][dades d'audio]
                // on `header_len` es la llargada del bloc de headers (excloent
                // el camp de 2 bytes) i les dades segueixen immediatament.
                if bin.len() < 2 {
                    return Err(VoiceError::Edge("missatge binari sense header length".into()));
                }
                let header_len = u16::from_be_bytes([bin[0], bin[1]]) as usize;
                let header_end = match 2usize.checked_add(header_len) {
                    Some(e) if e <= bin.len() => e,
                    _ => return Err(VoiceError::Edge("header binari inval·lid".into())),
                };
                let header = &bin[2..header_end];
                let is_audio = header
                    .windows(PATH_AUDIO.len())
                    .any(|w| w == PATH_AUDIO);
                if is_audio {
                    let chunk = &bin[header_end..];
                    if !chunk.is_empty() {
                        audio.extend_from_slice(chunk);
                        audio_received = true;
                    }
                }
            }
            Message::Ping(p) => {
                ws.send(Message::Pong(p)).await.ok();
            }
            Message::Close(_) => break,
            Message::Frame(_) | Message::Pong(_) => {}
        }
    }

    if !audio_received || audio.is_empty() {
        return Err(VoiceError::Edge(
            "no s'ha rebut audio del servei (potser el Sec-MS-GEC ha quedat obsolet: 403)".into(),
        ));
    }
    Ok(audio)
}

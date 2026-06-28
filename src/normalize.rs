use crate::error::{AppError, Result};

const TRACKING_PREFIXES: &[&str] = &["utm_"];
const TRACKING_KEYS: &[&str] = &["fbclid", "gclid", "mc_eid", "mc_cid", "igshid", "ref", "ref_src"];

/// Normalitza una URL: trim, lowercase, treu params de tracking, treu '/' final.
/// Valida que sigui http(s).
pub fn normalize_url(input: &str) -> Result<String> {
    let s = input.trim();
    if s.is_empty() {
        return Err(AppError::BadRequest("empty url".into()));
    }
    let lower = s.to_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(AppError::BadRequest("url must be http(s)".into()));
    }

    let (base, query) = match lower.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (lower.as_str(), None),
    };
    // Treu fragment del base.
    let base = base.split('#').next().unwrap_or(base);
    let base = base.trim_end_matches('/');

    let mut out = base.to_string();
    if let Some(q) = query {
        let q = q.split('#').next().unwrap_or(q);
        let kept: Vec<&str> = q
            .split('&')
            .filter(|pair| {
                let key = pair.split('=').next().unwrap_or("");
                !TRACKING_KEYS.contains(&key)
                    && !TRACKING_PREFIXES.iter().any(|p| key.starts_with(p))
                    && !key.is_empty()
            })
            .collect();
        if !kept.is_empty() {
            out.push('?');
            out.push_str(&kept.join("&"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tracking_and_trailing_slash() {
        let n = normalize_url("HTTPS://Example.com/Path/?utm_source=tw&id=5&fbclid=xx").unwrap();
        assert_eq!(n, "https://example.com/path?id=5");
    }

    #[test]
    fn dedup_equivalent() {
        let a = normalize_url("https://example.com/x ").unwrap();
        let b = normalize_url("https://example.com/x/").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn rejects_non_http() {
        assert!(normalize_url("ftp://x.com").is_err());
        assert!(normalize_url("  ").is_err());
    }
}

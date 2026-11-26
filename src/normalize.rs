use crate::error::{AppError, Result};

const TRACKING_PREFIXES: &[&str] = &["utm_"];
const TRACKING_KEYS: &[&str] = &["fbclid", "gclid", "mc_eid", "mc_cid", "igshid", "ref", "ref_src"];

/// Normalitza una URL: trim, treu params de tracking i '/' final, i passa a
/// minúscules NOMÉS l'esquema i el host.
///
/// El path i la query es conserven tal com arriben perquè són sensibles a
/// majúscules a molts llocs: IDs de YouTube (`watch?v=dQw4w9WgXcQ`), títols de
/// MediaWiki (`/wiki/Room_641A`), tags de GitHub (`releases/tag/Audacity-4.0.0`),
/// arxius mensuals (`2026-October`) o URLs signades. Passar-ho tot a minúscules
/// les trenca (404 i «video unavailable»).
///
/// Valida que sigui http(s).
pub fn normalize_url(input: &str) -> Result<String> {
    let s = input.trim();
    if s.is_empty() {
        return Err(AppError::BadRequest("empty url".into()));
    }
    let (scheme, rest) = s
        .split_once("://")
        .ok_or_else(|| AppError::BadRequest("url must be http(s)".into()))?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(AppError::BadRequest("url must be http(s)".into()));
    }

    // Host: fins al primer '/', '?' o '#'.
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (host, tail) = rest.split_at(host_end);
    if host.is_empty() {
        return Err(AppError::BadRequest("url must be http(s)".into()));
    }
    // Només el host va en minúscules (l'eventual userinfo es deixa intacte).
    let host = match host.rsplit_once('@') {
        Some((userinfo, h)) => format!("{userinfo}@{}", h.to_ascii_lowercase()),
        None => host.to_ascii_lowercase(),
    };

    // Fragment: fora. Query: es filtra però conservant majúscules i ordre.
    let before_frag = tail.split('#').next().unwrap_or(tail);
    let (path, query) = match before_frag.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (before_frag, None),
    };
    let path = path.trim_end_matches('/');

    let mut out = format!("{scheme}://{host}{path}");
    if let Some(q) = query {
        let kept: Vec<&str> = q
            .split('&')
            .filter(|pair| {
                // La clau es compara en minúscules (UTM_Source == utm_source),
                // però el valor es conserva tal com ve.
                let key = pair.split('=').next().unwrap_or("").to_ascii_lowercase();
                !key.is_empty()
                    && !TRACKING_KEYS.contains(&key.as_str())
                    && !TRACKING_PREFIXES.iter().any(|p| key.starts_with(p))
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
        assert_eq!(n, "https://example.com/Path?id=5");
    }

    #[test]
    fn keeps_path_and_query_case() {
        // El path i el valor de la query són significatius: no s'han de tocar.
        let yt = normalize_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap();
        assert_eq!(yt, "https://www.youtube.com/watch?v=dQw4w9WgXcQ");

        let wiki = normalize_url("https://en.wikipedia.org/wiki/Room_641A").unwrap();
        assert_eq!(wiki, "https://en.wikipedia.org/wiki/Room_641A");
    }

    #[test]
    fn lowercases_only_scheme_and_host() {
        let n = normalize_url("HTTPS://WWW.GitHub.COM/Astral-Sh/UV/Pull/21327/").unwrap();
        assert_eq!(n, "https://www.github.com/Astral-Sh/UV/Pull/21327");
    }

    #[test]
    fn dedup_equivalent() {
        let a = normalize_url("https://example.com/x ").unwrap();
        let b = normalize_url("https://example.com/x/").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn strips_fragment_and_tracking_case_insensitively() {
        let n = normalize_url("https://Example.com/a?UTM_Source=x&ID=AbC#frag").unwrap();
        assert_eq!(n, "https://example.com/a?ID=AbC");
    }

    #[test]
    fn keeps_bare_host_without_path() {
        assert_eq!(normalize_url("https://Example.com/").unwrap(), "https://example.com");
        assert_eq!(normalize_url("http://Example.com").unwrap(), "http://example.com");
    }

    #[test]
    fn rejects_non_http() {
        assert!(normalize_url("ftp://x.com").is_err());
        assert!(normalize_url("  ").is_err());
        assert!(normalize_url("example.com/x").is_err());
    }
}

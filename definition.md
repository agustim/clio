# Clio (LinkAnalyzer)

## 1. Resum Executiu
**LinkAnalyzer** és una aplicació escrita en Rust que recull enllaços d'internet a través de múltiples canals (API REST, Bot de Telegram, CLI), els analitza per extreure'n contingut, genera resums automàtics, classifica el tipus d'enllaç i assigna tags. El sistema gestiona usuaris i un mecanisme de **co-reporting** (si múltiples usuaris reporten la mateixa URL, es vinculen). Finalment, genera una web estàtica amb la informació processada i la desplega automàticament en un repositori Git.

## 2. Objectius Tècnics
1. **Rust Idiomatic**: Ús de `async/await`, `Result`/`Option`, i zero-cost abstractions.
2. **Modularitat**: Separació clara entre recepció, processament (pipeline), persistència i generació de web.
3. **Web Estàtica**: Generació de fitxers HTML/CSS/JS purs sense servidor backend per a la visualització.
4. **GitOps**: La web generada es commiteja i pusha automàticament a un repositori Git.
5. **Co-reporting**: Lligam automàtic d'usuaris que reporten el mateix URL.

## 3. Arquitectura de Components

### 3.1. Flux de Dades
El flux és el següent:
1. Entrada (API/Telegram/CLI) -> Validació i Deduplicació.
2. Si la URL no existeix -> Crear Link + Report.
3. Si la URL existeix -> Associar User al Link (Co-report).
4. Pipeline Async: Fetch -> Parse -> LLM -> Tags.
5. Guardar a SQLite.
6. Trigger: Generar Web Estàtica.
7. Commit & Push a Git.

### 3.2. Estratificació
1. **Presentation Layer**: `axum` (API), `telegram-bot` (Bot), `clap` (CLI).
2. **Application Layer**: Gestió de transaccions, coordinació del pipeline async.
3. **Domain Layer**: Regles de negoci (deduplicació, classificació, sentiment).
4. **Infrastructure Layer**: SQLite (`sqlx`), HTTP (`reqwest`), Git (`git2`), LLM Clients.

## 4. Model de Dades (SQLite)

### 4.1. Esquema SQL
Executar aquestes migrations inicials:

```sql
-- users table
CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,          -- UUID v4
    username TEXT UNIQUE NOT NULL,
    api_token TEXT UNIQUE NOT NULL,
    role TEXT DEFAULT 'user' CHECK(role IN ('admin', 'user')),
    created_at TEXT DEFAULT CURRENT_TIMESTAMP
);

-- links table (Core entity)
CREATE TABLE IF NOT EXISTS links (
    id TEXT PRIMARY KEY,          -- UUID v4
    url TEXT UNIQUE NOT NULL,     -- URL normalitzada
    title TEXT,
    summary TEXT,                 -- Resum generat
    link_type TEXT DEFAULT 'other' CHECK(link_type IN ('news', 'repo', 'article', 'video', 'blog', 'other')),
    tags TEXT DEFAULT '[]',       -- JSON Array: ["rust", "async"]
    sentiment TEXT DEFAULT 'neutral' CHECK(sentiment IN ('positive', 'neutral', 'negative')),
    status TEXT DEFAULT 'pending' CHECK(status IN ('pending', 'processing', 'done', 'failed')),
    co_reporters TEXT DEFAULT '[]', -- JSON Array of user IDs who reported this
    created_at TEXT DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT DEFAULT CURRENT_TIMESTAMP
);

-- reports table (Audit trail & Linking)
CREATE TABLE IF NOT EXISTS reports (
    id TEXT PRIMARY KEY,
    link_id TEXT REFERENCES links(id),
    user_id TEXT REFERENCES users(id),
    status TEXT DEFAULT 'pending',
    created_at TEXT DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(link_id, user_id) -- Un usuari només pot reportar un link un cop
);
```

### 4.2. Estructures Rust (Rust Types)

```rust
use uuid::Uuid;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: Uuid,
    pub username: String,
    pub role: UserRole,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum UserRole {
    Admin,
    User,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum LinkType {
    News,
    Repo,
    Article,
    Video,
    Blog,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Sentiment {
    Positive,
    Neutral,
    Negative,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum LinkStatus {
    Pending,
    Processing,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub id: Uuid,
    pub url: String,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub link_type: LinkType,
    pub tags: Vec<String>,
    pub sentiment: Sentiment,
    pub status: LinkStatus,
    pub co_reporters: Vec<Uuid>, // IDs dels usuaris que ho han reportat
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub id: Uuid,
    pub link_id: Uuid,
    pub user_id: Uuid,
    pub status: ReportStatus,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ReportStatus {
    Pending,
    Processed,
    Failed,
}
```

## 5. Regles de Negoci i Lògica

### 5.1. Normalització i Deduplicació
- **URL Normalitzada**: `url.trim().to_lowercase()` + eliminació de paràmetres de tracking si cal.
- **Deduplicació**: Si la URL normalitzada existeix a la taula `links`, no es crea un nou link. En lloc d'això:
    1. S'afegeix el `user_id` a l'array JSON `co_reporters` (si no hi és).
    2. S'actualitza `updated_at`.
    3. S'actualitza l'estat del `report` associat.
    4. Si el link estava `pending` o `processing`, es reinicia o continua el processament.

### 5.2. Pipeline de Processament (Async)
El processament ha de ser asíncron i no bloquejant. S'utilitza un `tokio::task` o un canal d'episodis.

1. **Fetch**: `reqwest` amb User-Agent personalitzat. Límit de mida: 5MB. Timeout: 10s.
2. **Parse**: Extracció de text utilitzant `readability` o `html5ever`. Netega HTML tags innecessaris.
3. **Classify**:
   - Heurística: Detectar si és un repo (URL conté `github.com`, `gitlab.com`).
   - Detectar si és notícia (meta tags `og:type="article"`).
   - Per defecte: `Other`.
4. **Summarize**:
   - Input: Text net (max 4000 chars).
   - Output: String < 300 paraules.
   - *Nota IA*: Implementar trait `Summarizer` amb fallback: si no hi ha LLM, fer un extracte de les primeres 3 frases.
5. **Tagging**:
   - Extracció de keywords del títol i resum.
   - Normalització a minúscules i sense accents.
   - Límit: 5-10 tags màxim.
6. **Sentiment**:
   - Anàlisi bàsic o via LLM.
   - Output: `Positive`, `Neutral`, `Negative`.

### 5.3. Co-reporting
- Quan un usuari reporta un link nou, el camp `co_reporters` conté `[user_id]`.
- Quan un altre usuari reporta el mateix link, `co_reporters` es converteix en `[user_id_1, user_id_2]`.
- La web mostrarà: "Reportat per 2 usuaris" o similar.

## 6. Generació de Web Estàtica

### 6.1. Requisits
- **Tecnologia**: Templates `askama` o generació manual de HTML/JS.
- **Sortida**: Carpeta `./public/`.
- **Fitxers generats**:
  - `index.html`: Pàgina principal amb grid de targetes.
  - `data/links.json`: JSON amb tots els links processats (per a filtrat client-side).
  - `css/style.css`: Estils minimalistes, responsive.
  - `js/app.js`: Lògica de filtrat, cerca i renderitzat de targetes des del JSON.

### 6.2. Estructura de la Targeta (Card)
Cada link es representa amb una targeta HTML que conté:
- Títol (enllaç a l'URL original).
- Tipus d'enllaç (badge: `News`, `Repo`, etc.).
- Resum (truncat si és massa llarg).
- Tags (chips clicables per filtrar).
- Sentiment (icon o color).
- Número de reporters (ex: "👥 3").

### 6.3. Integració amb Git
- Després de generar els fitxers a `./public/`:
  1. Executar `git add .` dins del directori `public/` (o un repo separat si es prefereix).
  2. Faser commit amb missatge: `chore: update static web for link {id}`.
  3. Fer `git push origin main`.
- *Nota*: Si la web està en el mateix repo que el codi, caldrà gestionar branches separades (ex: `main` per codi, `gh-pages` o `static` per la web) o usar un repo separat per a la web. **Recomanació**: Configurar un repositori Git separat per a la web (`web_repo_url`).

## 7. Interfícies (API & Bot)

### 7.1. API REST (Axum)
Base URL: `/api/v1`

| Mètode | Ruta | Descripció | Body |
|--------|------|------------|------|
| POST | `/links` | Reportar un nou link | `{ "url": "https://...", "token": "..." }` |
| GET | `/links` | Llistar links | Query params: `?tag=rust&limit=20&sentiment=positive` |
| GET | `/links/{id}` | Detall d'un link | - |
| GET | `/stats` | Estadístiques globals | - |

*Autenticació*: Header `Authorization: Bearer <api_token>`.

### 7.2. Telegram Bot
- **Comandes**:
  - `/start`: Mostra instruccions i genera un token d'API temporal (o demana registre).
  - `/add <url>`: Reporta un link.
  - `/list`: Mostra els últims 5 links reportats per l'usuari.
  - `/help`: Mostra ajuda.
- **Respostes**:
  - Confirmació amb targeta inline (si està processat) o missatge "Processant...".

### 7.3. CLI
- `linkanalyzer add <url>`: Afegeix un link (usa credencials locals o `.env`).
- `linkanalyzer list`: Mostra llista per terminal.
- `linkanalyzer generate`: Força la generació de web estàtica.
- `linkanalyzer push`: Força el push a Git.

## 8. Stack Tècnic Recomanat

| Component | Crate / Eina |
|-----------|--------------|
| Runtime | `tokio` |
| Web Framework | `axum`, `tower-http` |
| DB | `sqlx` (SQLite mode), `serde`, `serde_json` |
| HTTP Client | `reqwest` |
| HTML Parsing | `scraper` o `readability-rs` |
| Templates Web | `askama` (opcional) o manual |
| Git Integration | `git2` |
| CLI | `clap` |
| Telegram Bot | `telegram-bot` o `teloxide` |
| Logging | `tracing`, `tracing-subscriber` |
| Config | `dotenvy`, `config` |

## 9. Instruccions per a la IA (Pas a Pas)

Per generar aquest projecte, segueix aquest ordre estricte:

1. **Inici del Projecte**:
   - Crea un projecte Rust amb `cargo init`.
   - Afegeix les dependències a `Cargo.toml`.

2. **Configuració i Models**:
   - Implementa `config.rs` per llegir `.env`.
   - Crea les estructures de dades (`models/`) amb `serde`.

3. **Base de Dades**:
   - Implementa `db.rs` amb connexió a SQLite.
   - Crea funcions per inserir/actualitzar `links` i `reports`.
   - Implementa la lògica de **co-reporting** aquí (check if exists, update array).

4. **Pipeline de Processament**:
   - Crea un mòdul `pipeline/` amb traits o structs separats per `fetch`, `parse`, `summarize`, `tag`.
   - Implementa la lògica asíncrona. Per ara, fes que el `summarize` i `tag` retornin valors simulats o bàsics si no hi ha LLM configurat.

5. **API REST**:
   - Implementa els endpoints d'`axum`.
   - Connecta amb la DB i el pipeline.

6. **Generador de Web**:
   - Crea un mòdul `webgen/`.
   - Genera `index.html` amb un bucle que llegeix de `links.json`.
   - Afegeix CSS/JS bàsic per al filtrat.

7. **Integració Git**:
   - Implementa la funció que fa commit i push a la carpeta `public/`.

8. **Bot i CLI**:
   - Afegeix les interfícies de Telegram i CLI que cridin als mateixos serveis de l'API.

9. **Tests**:
   - Afegeix tests unitaris per a la deduplicació i el parsing.

## 10. Exemple de `.env`

```env
# Database
DATABASE_URL=sqlite://data/linkanalyzer.db

# LLM (Optional, si no es posa, usar fallback bàsic)
LLM_PROVIDER=local # openai, ollama, local
LLM_MODEL=gpt-3.5-turbo
LLM_API_KEY=sk-...

# Git Repo per a la web estàtica
WEB_REPO_URL=https://github.com/user/linkanalyzer-web.git
WEB_BRANCH=main
GIT_TOKEN=ghp_...

# Telegram
TELEGRAM_BOT_TOKEN=123456:ABC-DEF...
ADMIN_CHAT_ID=-100...

# Limits
MAX_LINK_SIZE_MB=5
SUMMARY_MAX_WORDS=300
```

## 11. Constraints i Qualitat
- **Error Handling**: No usar `unwrap()`. Ús exhaustiu de `?` i `match`.
- **Logging**: Usar `tracing` per a depuració.
- **Seguretat**: Validar sempre les URLs entrants. No executar scripts de la web generada.
- **Idioma**: La documentació i els missatges d'error poden estar en anglès, però la UI web i els resums han de ser en català (o detectat automàticament).

---

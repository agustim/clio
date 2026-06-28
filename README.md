# Clio · LinkAnalyzer

Recull enllaços (API REST / CLI / Telegram-stub), els analitza (fetch → parse → classify → summarize → tags → sentiment), els desa a SQLite amb **co-reporting**, i genera una web estàtica publicable per Git.

Implementació de [definition.md](definition.md). Abast actual: **nucli sòlid** — API + CLI + pipeline + webgen complets; Telegram és un stub funcional; git push és opt-in.

## Arquitectura

| Capa | Fitxer |
|------|--------|
| Config (`.env`) | [src/config.rs](src/config.rs) |
| Models + enums | [src/models.rs](src/models.rs) |
| DB + co-reporting | [src/db.rs](src/db.rs) |
| Normalització/dedup URL | [src/normalize.rs](src/normalize.rs) |
| Pipeline async (1a passada) | [src/pipeline.rs](src/pipeline.rs) |
| Cua de workers | [src/queue.rs](src/queue.rs) |
| 2a passada (deep) | [src/deep.rs](src/deep.rs) |
| Client LLM (OpenAI-compat) | [src/llm.rs](src/llm.rs) |
| Orquestració (AppState) | [src/service.rs](src/service.rs) |
| API REST (axum) | [src/api.rs](src/api.rs) |
| Web estàtica + git push | [src/webgen.rs](src/webgen.rs) |
| CLI (clap) | [src/cli.rs](src/cli.rs) |

## Quickstart

```bash
cp .env.example .env          # ajusta valors
cargo build
DB=target/debug/linkanalyzer

$DB user-add alice --admin    # crea usuari -> imprimeix api_token
$DB add https://www.rust-lang.org
$DB list
$DB generate                  # escriu ./public (index.html, data/links.json, css, js)
$DB serve                     # API a http://127.0.0.1:8080
```

## LLM (vLLM / OpenAI / Ollama)

Endpoint compatible OpenAI (`/v1/chat/completions`). Config a `.env`:

```env
LLM_PROVIDER=vllm
LLM_MODEL=Qwen/Qwen2.5-7B-Instruct
LLM_BASE_URL=http://localhost:8000/v1
LLM_API_KEY=        # opcional (vLLM local sovint no en cal)
```

Si l'endpoint no respon o `LLM_PROVIDER=none`, s'usa **fallback extractiu** (3 primeres frases + tags per freqüència + sentiment per lèxic). El resum es demana en català.

## Cua d'anàlisi + segona passada (deep)

Quan s'encua una URL es processa en **dues fases asíncrones**:

1. **Shallow** (1a passada): fetch → parse → classify → analyze (resum, tags, sentiment).
2. **Deep** (2a passada, auto-encuada si aplica):
   - **Repos** (`github/gitlab/...`): `git clone --depth 1 --no-recurse-submodules` a un tmp, escaneig de codi (llenguatges, LOC, fitxers, README) → anàlisi tècnica LLM. `code_stats` (JSON) i `deep_summary` es desen. Tmp s'esborra sempre (RAII guard). Límits: `CLONE_TIMEOUT_SECS`, `CLONE_MAX_MB`.
   - **Articles/blogs/news**: re-fetch del text complet (no truncat) → resum llarg.

Arquitectura: worker pool amb `tokio::mpsc` + `Semaphore(QUEUE_WORKERS)` ([src/queue.rs](src/queue.rs)). En arrencar `serve`, **recovery** re-encua la feina pendent/encallada de la DB (`status`/`deep_status` a pending/processing/failed). La CLI `add` processa inline (shallow+deep) per mostrar el resultat a l'instant.

```env
QUEUE_WORKERS=4
CLONE_TIMEOUT_SECS=120
CLONE_MAX_MB=200
```

## API REST (`/api/v1`)

Auth: `Authorization: Bearer <api_token>`.

| Mètode | Ruta | |
|--------|------|--|
| POST | `/links` | `{"url":"https://…"}` → encua processament |
| GET | `/links` | `?tag=&sentiment=&link_type=&limit=` |
| GET | `/links/{id}` | detall |
| GET | `/stats` | comptadors globals |

## Git push (opt-in)

`generate` sempre escriu `./public`. `push` fa commit+push **només** si `WEB_REPO_URL` està definit (init, remote amb `GIT_TOKEN`, `git push origin <WEB_BRANCH>`). Sense config → s'omet amb log. Usa el `git` del sistema (no `git2`) per evitar dependències natives pesades.

## Tests

```bash
cargo test    # normalització/dedup + classify + parse + fallback
```

## Pendent (fora d'abast actual)

- Bot Telegram real (stub a [src/telegram.rs](src/telegram.rs), dissenyat per reusar `AppState::report_link` amb `teloxide`).
- Trigger automàtic de webgen+push després de cada processament (ara manual via CLI).

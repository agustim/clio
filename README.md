# Clio · LinkAnalyzer

Recull enllaços (API REST / CLI / bot de Telegram), els analitza (fetch → parse → classify → summarize → tags → sentiment), genera **embeddings** per a ranking personalitzat, els desa a SQLite amb **co-reporting**, i genera una web estàtica publicable per Git (deploy reactiu opt-in).

Implementació de [definition.md](definition.md). Abast actual: API + CLI + pipeline (shallow+deep) + embeddings + bot de Telegram + webgen complets; git push i deploy reactiu són opt-in.

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
| Embeddings (local/HTTP) | [src/embed.rs](src/embed.rs) |
| Extractors xarxes socials | [src/social.rs](src/social.rs) |
| Col·lectors NPC (RSS) | [src/feeds.rs](src/feeds.rs) |
| Bot de Telegram | [src/telegram.rs](src/telegram.rs) |
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
$DB reindex                   # backfill d'embeddings dels links existents
$DB serve                     # API + bot a http://127.0.0.1:8080
```

Subcomandes: `serve`, `user-add`, `add`, `list`, `generate`, `reindex`, `reprocess`, `delete`, `push`, `npc-add`, `feed-add`, `feed-list`.

## LLM (vLLM / OpenAI / Ollama)

Endpoint compatible OpenAI (`/v1/chat/completions`). Config a `.env`:

```env
LLM_PROVIDER=vllm
LLM_MODEL=Qwen/Qwen2.5-7B-Instruct
LLM_BASE_URL=http://localhost:8000/v1
LLM_API_KEY=        # opcional (vLLM local sovint no en cal)
```

Si l'endpoint no respon o `LLM_PROVIDER=none`, s'usa **fallback extractiu** (3 primeres frases + tags per freqüència + sentiment per lèxic). El resum es demana en català.

## Embeddings (ranking personalitzat)

Independents del LLM de chat. Habiliten ranking per "cors" a la web. Backfill: `linkanalyzer reindex`.

```env
EMBED_PROVIDER=local                 # local | openai | ollama/vllm/http | (buit=reusa LLM_PROVIDER)
EMBED_MODEL=multilingual-e5-small    # en local: id de fastembed (bge-m3, nomic-embed-text…)
EMBED_DIM=256
EMBED_BASE_URL=                       # buit => reusa LLM_BASE_URL
EMBED_API_KEY=
```

- `local`: in-process via **fastembed** (feature `local-embed`, activa per defecte). Descarrega el model el primer cop, després offline. Cau a `.fastembed_cache`.
- `openai` / `ollama` / `vllm` / `http`: endpoint OpenAI-compatible (`/embeddings`).

Build lleuger sense embeddings locals: `cargo build --no-default-features` (només via HTTP).

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

### Pàgines protegides (Cloudflare & co.)

Alguns llocs responen `403/429/503` a un GET normal (murs anti-bot). Clio fa dues
coses:

1. **Capçaleres de navegador** per defecte (UA de Chrome + `Accept`/`Sec-Fetch-*`).
   Passa la majoria de filtres senzills. Sobreescriu el UA amb `USER_AGENT`.
2. **Fallback FlareSolverr** per als challenges reals de Cloudflare (JS/Turnstile):
   si `FLARESOLVERR_URL` està definida, un `403/429/503` es reintenta a través d'un
   navegador headless que resol el challenge. El `docker-compose.yml` ja inclou el
   servei `flaresolverr`.

```env
# Buit = desactivat (els 403 fallen directament).
FLARESOLVERR_URL=http://flaresolverr:8191
FLARESOLVERR_TIMEOUT_SECS=60
```

## API REST (`/api/v1`)

Auth: `Authorization: Bearer <api_token>`.

| Mètode | Ruta | |
|--------|------|--|
| POST | `/links` | `{"url":"https://…"}` → encua processament |
| GET | `/links` | `?tag=&sentiment=&link_type=&limit=` |
| GET | `/links/{id}` | detall |
| GET | `/stats` | comptadors globals |

## Bot de Telegram

Si `TELEGRAM_BOT_TOKEN` està definit, `serve` arrenca el bot. Accepta links d'usuaris amb `telegram_id` a la fitxa i respon "Processant url." en encuar-los. `ADMIN_CHAT_ID` rep avisos d'admin (arrencada + errors d'anàlisi); buit = cap avís. L'id numèric el dóna `@userinfobot` (grups: id negatiu).

```env
TELEGRAM_BOT_TOKEN=
ADMIN_CHAT_ID=
```

## Col·lectors NPC (RSS)

Un **NPC** és un usuari automàtic (`role = npc`) que recull enllaços de fonts externes i els reporta pel **mateix camí** que qualsevol usuari (`report_link` → dedup → co-reporting → pipeline). No cal codi nou al pipeline: els seus links s'analitzen, resumeixen i indexen igual, i apareix com a reporter (`@npcname`) a la web.

Cada NPC té un o més **feeds** (taula `feeds`): una font + un període. El scheduler ([src/feeds.rs](src/feeds.rs)), que arrenca amb `serve`, revisa cada 60 s els feeds habilitats i col·lecta els que han vençut (`now - last_run >= interval_s`). Màxim 25 entrades per col·lecta (evita inundar el ranking); `last_run` es marca encara que falli (no reintenta en bucle).

```bash
linkanalyzer npc-add hackernews                                         # crea NPC -> imprimeix api_token
linkanalyzer feed-add hackernews https://hnrss.org/frontpage --interval 1800   # feed RSS/Atom (segons)
linkanalyzer feed-list                                                  # llista feeds
linkanalyzer serve                                                      # el scheduler arrenca sol
```

Dedup i co-reporting són automàtics: si un feed re-veu un link ja existent, s'afegeix l'NPC com a co-reporter (no es duplica). El feed pren el primer `<link>` de cada entrada.

**Fase 2 (pendent): scrape.** `FeedKind::Scrape` i la columna `config_json` ja estan reservats. La idea: `pipeline::fetch` (amb fallback FlareSolverr) baixa l'HTML i una passada d'IA el converteix en notícies. Encara no implementat.

## Git push + deploy reactiu (opt-in)

`generate` sempre escriu `./public`. `push` fa commit+push **només** si `WEB_REPO_URL` està definit (init, remote amb `GIT_TOKEN`, `git push origin <WEB_BRANCH>`). Sense config → s'omet amb log. Usa el `git` del sistema (no `git2`) per evitar dependències natives pesades.

Durant `serve`, dues estratègies de regeneració de la web:

```env
WEB_REGEN_SECS=0      # regeneració periòdica (segons). 0 = desactiva.
WEB_DEBOUNCE_SECS=60  # deploy reactiu: agrupa una ràfega de links nous en un sol push
```

Recomanat: `WEB_REGEN_SECS=0` + deploy reactiu — la web es regenera i fa push només quan la cua acaba d'analitzar links nous (debounce per agrupar ràfegues).

## Docker

Imatge mínima multi-stage ([Dockerfile](Dockerfile)):

```bash
docker build -t clio .
docker run -p 8080:8080 \
  -v $PWD/data:/app/data \
  -v $PWD/.fastembed_cache:/app/.fastembed_cache \
  --env-file .env clio
```

`BIND_ADDR` per defecte dins la imatge és `0.0.0.0:8080` (cal per accedir des de fora del container). Munta `data/` per persistir SQLite i `.fastembed_cache/` per no re-descarregar el model d'embeddings.

## Releases (CI)

`scripts/release.sh [patch|minor|major]` puja un tag `vX.Y.Z` que dispara [.github/workflows/release.yml](.github/workflows/release.yml):

1. Build de l'executable + GitHub Release (marcada `latest`).
2. Container a `ghcr.io/agustim/clio:vX.Y.Z` i `:latest`.
3. Neteja: manté les 5 últimes releases i packages.

## Tests

```bash
cargo test    # normalització/dedup + classify + parse + fallback
```

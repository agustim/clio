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
$DB generate                  # escriu ./public (index.html, data/manifest.json + shards, css, js)
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

Amb el LLM configurat, el títol, la descripció curta i la profunda es generen **sempre en català** (és una exigència del prompt). Les crides al model es **reintenten** davant errors transitoris; si al final tampoc respon (o respon buit), el link queda com a `failed` (marcat «⚠ Revisa» a la web, reintentable amb «↻ Refer») en lloc de publicar un text en l'idioma original de la pàgina. Només amb `LLM_PROVIDER=none` (sense model) s'usa el **fallback extractiu** (3 primeres frases + tags per freqüència + sentiment per lèxic), que copia l'idioma de la font.

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

## Web estàtica: fonts seguides i disposició de dades

La web gira al voltant de **seguir fonts** (usuaris i NPCs): cada visitant tria quines segueix (cookie al navegador) i només es baixen i mostren els seus links. Les dades es publiquen per font, mes i part per escalar amb molts links:

- `data/manifest.json` — punt d'entrada: total + fonts (`name`, `dir`, `role`, `total`, mesos amb `parts`) + categories.
- `data/u/{font}/{YYYY-MM}-p{N}.json` — índex lleuger per font, mes i part (~200 links/part). Un link co-reportat apareix al shard de cada reporter; el client dedupa per id.
- `data/u/{font}/emb-{YYYY-MM}-p{N}.json` — embeddings quantitzats alineats al part; només es baixen si es fan servir els cors.
- `data/i/{id}.json` — fitxa lleugera per enllaç (permalinks `#id:` de qualsevol font, seguida o no).
- `data/deep/{id}.json` — resum profund, carregat en obrir l'anàlisi.
- `data/links.json` + `data/links.js` — índex lleuger complet: consum extern i fallback per a `file://` (on fetch està bloquejat, app.js injecta `links.js`; en aquest mode no hi ha fonts ni historial per mesos).
- `img/{id}.{ext}` — còpia de les imatges d'acompanyament de les cards (les mateixes que es desen a `IMAGES_DIR` per a l'overlay). Cada card mostra la seva imatge (camp relatiu `img` a l'índex); si no hi ha còpia local, cau a `/imgout/{id}` (API) o al proxy `/img?u=`.

**UI**: chip `👥 N fonts` (o menú «Fonts que segueixes») obre el selector de categories i fonts; chip `📅 fins <mes>`, menú «Historial: un mes més/menys» i botó al final de la graella estiren l'historial. La graella pinta ~60 cards i creix amb scroll (render incremental: no es construeixen milers de cards de cop).

### Categories (`.env`)

```env
WEB_CATEGORIES=general=clio,hackernews;tecnologia=wired,ars-technica
WEB_DEFAULT_CATEGORY=general
```

`WEB_DEFAULT_CATEGORY` és el que veu un visitant nou sense selecció (buida = totes les fonts). Truc per a l'onboarding: crea un usuari `clio` que reporti enllaços de documentació (com fer servir la web, els cors, les categories) i posa'l a la categoria per defecte:

```bash
linkanalyzer user-add clio
linkanalyzer add https://github.com/agustim/clio#readme   # amb el token de clio via API, o des del CLI
```

## Git push + deploy reactiu (opt-in)

`generate` sempre escriu `./public`. `push` fa commit+push **només** si `WEB_REPO_URL` està definit (init, remote amb `GIT_TOKEN`, `git push origin <WEB_BRANCH>`). Sense config → s'omet amb log. Usa el `git` del sistema (no `git2`) per evitar dependències natives pesades.

Durant `serve`, dues estratègies de regeneració de la web:

```env
WEB_REGEN_SECS=0      # regeneració periòdica (segons). 0 = desactiva.
WEB_DEBOUNCE_SECS=60  # deploy reactiu: agrupa una ràfega de links nous en un sol push
```

Recomanat: `WEB_REGEN_SECS=0` + deploy reactiu — la web es regenera i fa push només quan la cua acaba d'analitzar links nous (debounce per agrupar ràfegues).

## Overlay de directe i emissió a Twitch

Clio pot generar un **canal de notícies en directe** a partir del mateix contingut que ja
recull: una escena HTML amb **bloc superior** (logo + rellotge/data en directe i segell
"EN DIRECTE"), **zona central** de cards de notícies que roten automàticament — on el cos
de cada notícia és **l'anàlisi profunda del LLM** (`deep_summary`, netejada de markdown),
amb títol, **tipus de notícia** (etiqueta en català), **dia i hora de la notícia**, font
`@reporter` i **imatge d'acompanyament amb crèdit** — i **bloc
inferior** amb el *news crawl* de titulars.

- `/overlay` — l'escena HTML (font de navegador per a OBS, o per al mode headless).
- `/overlay/ticker.json` — les últimes N notícies **amb anàlisi profunda**
  (`deep_status='done'` i `deep_summary` no buit; les que no l'han assolit no
  es mostren a l'overlay), limitades per `OVERLAY_MAX_ITEMS` (per defecte 50),
  amb `image` apuntant a la còpia local o al proxy `/img`.
- `/imgout/{id}` — serveix la **còpia local** de la imatge (desada a `IMAGES_DIR`,
  per defecte `data/images/`).
- `/img?u=...` — proxy d'imatges remot (anti-hotlink, amaga el referrer, guardes SSRF).

Comportament de l'escena: la graella mostra `OVERLAY_CARDS` cards i **salta de CARDS en
CARDS** a cada canvi (`OVERLAY_ROTATE_SECS`) — totes canvien a l'hora —; cada card mostra
fins a `OVERLAY_TEXT_LINES` línies (per defecte 9) de l'anàlisi del LLM, donant
prioritat al text i limitant l'alçada de la imatge (`height: clamp(110px, 17vh, 180px)`).

Les **hores/data de les notícies i el rellotge** es mostren a la **TimeZone configurable**
`OVERLAY_TIMEZONE` (IANA, p.ex. `Europe/Andorra`); si va buida s'usa l'hora local del
navegador que emet el stream. Les notícies publicades durant la **finestra `OVERLAY_NEW_MINUTES`**
(per defecte 30 minuts; `0` = desactiva) es marquen com a **NOU**: la card rep un **marc
vermell** amb pols suau i un cintó `● NOU`, i al *news crawl* apareix la mateixa etiqueta.

Les imatges s'extreuen del `og:image` (o primera imatge) de cada article a la 1a passa i,
**en analitzar la cua, es baixen i es desen localment** a `IMAGES_DIR` com a
`<link_id>.<ext>`: l'overlay les serveix des de `/imgout/{id}` (ràpid, sense dependre del
servidor original i resistent a hotlink/imatges mortes). Si el download falla o no és una
imatge, es torna al proxy remot `/img`. Backfill per als links ja processats, **sense
re-analitzar res**:

```bash
linkanalyzer images --limit 200   # 1) extreu og:image pels que en manquin; 2) baixa còpies locals
```

> Política mixta recomanada: `og:image` de l'article **sempre amb crèdit** (`📷 domini` a
> la card) i, si no n'hi ha, placeholder del tema. Si vols imatges lliures de risc, pots
> canviar el fallback a Wikimedia/Openverse (vegeu `src/overlay.rs`).

### Emetre amb OBS (ràpid, semiautomàtic)

1. `linkanalyzer serve` (o el desplegament).
2. OBS → Font de navegador → `http://127.0.0.1:8080/overlay` (1280×720).
3. Afegeix una font de navegador per al directe i *Inicia emissió* a Twitch.

### Emetre headless 24/7 (sense OBS)

El mode `stream` (Xvfb + Chromium + ffmpeg → RTMP) automatitza el directe del tot.
**Un mateix [Dockerfile](Dockerfile) multietapa construeix totes dues imatges**, sense
dependre d'imatges base pròpies ja publicades:

| Target | Imatge | Contingut |
|---|---|---|
| `--target app` (per defecte a la imatge `clio`) | `ghcr.io/agustim/clio` | lleugera: només `clio serve` (API + web) |
| `--target stream` (per defecte de `docker build .`) | `ghcr.io/agustim/clio-stream` | **«tot en un»**: clio **+** emissió; tria el mode amb `CLIO_MODE` |

La imatge «tot en un» executa qualsevol de les dues coses (a més de tota la CLI):

```bash
docker run clio-stream                 # CLIO_MODE buit  -> serve (com `clio`)
docker run -e CLIO_MODE=stream ...     #                  -> emissió cap a Twitch
docker run clio-stream images --limit 200   # qualsevol subordre CLI directe
```

Per aixecar el directe automatitzat (fent servir les imatges de ghcr.io):

```bash
cp .env.example .env    # posa-hi TWITCH_STREAM_KEY=...
docker compose -f docker-compose.stream.yml pull && docker compose -f docker-compose.stream.yml up -d
```

- `docker-compose.stream.yml` — per defecte usa les imatges publicades
  `ghcr.io/agustim/clio` (servei `clio`) i `ghcr.io/agustim/clio-stream`
  (servei `stream`, `CLIO_MODE=stream`). Els blocs `build` hi queden comentats
  per si vols construir-les des d'aquest repo.
- `scripts/stream.sh` — captura l'overlay i el puja a Twitch, reiniciant ffmpeg si la
  connexió cau. Usa un **perfil de Chromium fresc a cada arrencada** (`CHROME_PROFILE`,
  per defecte `/tmp/clio-chrome`) amb `--no-first-run`, `--no-default-browser-check`
  i `--disable-features=Translate`: garanteix que la finestra kiosk obri només l'overlay,
  sense cap element de Chrome (finestra de navegador per defecte, traducció, pestanya
  buida "This space intentionally blank…"). Després de tocar-lo cal
  `docker compose -f docker-compose.stream.yml up -d --build stream`.
- Variables: `OVERLAY_URL`, `TWITCH_RTMP_URL` (default `rtmp://live.twitch.tv/app`),
  `OVERLAY_WIDTH/HEIGHT/FPS`, `VIDEO_BITRATE` (recomanat 5000k).
  **Tingues en compte**: el Chromium kiosk força la finestra a 1920×1080. Si la pantalla
  virtual fos més petita, el contingut es retallaria i es veuria encongit — per això
  l'escena es captura a 1080p (1920×1080) per defecte. Si depasses Twitch amb `rtmp://`
  directe, recorda que el límit màxim de bitrate és ~6000 kbps.

<br>

> **Avís (contingut de tercers)**: els titulars i resums amb **font visible** són citació
> legítima, però les **imatges** pertanyen a qui les crea. Mostrar `og:image` amb crèdit
> és el que fan els agregadors i té risc baix a la pràctica, però no l'elimina del tot
> (DMCA). No reprodueixis vídeos sencers aliens (banda ampla + rebroadcast prohibit);
> mostra miniatura/enllaç al canal original.

## Docker

[Dockerfile](Dockerfile) multietapa amb dos targets (tot es construeix des d'aquest repo):

```bash
# Imatge lleugera (clio serve) — la que puja a ghcr.io/agustim/clio
docker build --target app -t clio .
# Imatge "tot en un" (serve + stream + CLI) — ghcr.io/agustim/clio-stream
docker build --target stream -t clio-stream .

# Ús bàsic (qualsevol de les dues): serve + web a :8080, amb data persistida
docker run -p 8080:8080 \
  -v $PWD/data:/app/data \
  -v $PWD/.fastembed_cache:/app/.fastembed_cache \
  --env-file .env clio
# Mode emissió (només la "tot en un"): CLIO_MODE=stream + TWITCH_STREAM_KEY
docker run --rm -e CLIO_MODE=stream --env-file .env clio-stream
```

`BIND_ADDR` per defecte dins la imatge és `0.0.0.0:8080` (cal per accedir des de fora del container). Munta `data/` per persistir SQLite (i `data/images/`, les imatges baixades) i `.fastembed_cache/` per no re-descarregar el model d'embeddings.

## Releases (CI)

`scripts/release.sh [patch|minor|major]` puja un tag `vX.Y.Z` que dispara [.github/workflows/release.yml](.github/workflows/release.yml):

1. Build de l'executable + GitHub Release (marcada `latest`).
2. Container a `ghcr.io/agustim/clio:vX.Y.Z` i `:latest`.
3. Neteja: manté les 5 últimes releases i packages.

## Tests

```bash
cargo test    # normalització/dedup + classify + parse + fallback
```

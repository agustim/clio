#!/usr/bin/env bash
# Clio Overlay -> Twitch i/o YouTube (sense OBS).
#
# Cadenes de 24/7: Xvfb (display virtual) + Chromium (escena /overlay) capturats
# per ffmpeg (x11grab) i pujats per RTMP.
#
# STREAM_MODE determina COM s'emet a les plataformes amb clau configurada:
#   tee   (per defecte) UNA sola codificació: ffmpeg codifica una vegada i el
#         muxer `tee` fa arribar EL MATEIX flux a totes les plataformes. No es
#         codifica mai dues vegades (estalvia CPU/GPU). Un watcher vigila el
#         log de ffmpeg: si una plataforma cau a mig directe, força el reinici
#         per reconectar-la; si una plataforma ja era avall en arrencar, hi
#         torna cada TEE_RETRY_SECS sense fer flap (mira TEE_WARMUP_SECS).
#   split Cadascuna en el SEU procés ffmpeg independent: útil si vols que una
#         caiguda no afecti mai les altres o si vols bitrates diferents per
#         plataforma. Consumeix el DOBLE de CPU en mode software (amb GPU/VAAPI
#         gairebé no es nota).
#
# Config (variables d'entorn):
#   OVERLAY_URL      (default http://127.0.0.1:8080/overlay)
#   CHROME_PROFILE   (perfil de Chromium dedicat; es buida a cada arrencada.
#                     Evita pestanyes bufades i finestres de Chrome: "posar com
#                     a navegador per defecte", traducció, restauració de sessió,
#                     "This space intentionally blank…", etc.)
#   STREAM_MODE      tee | split (per defecte tee; vegeu a dalt).
#   TEE_WARMUP_SECS  Mode tee: segons que passen abans d'actuar sobre un "slave
#                    failed". Una plataforma que falla DINS aquesta finestra
#                    s'interpreta com "ja era avall en arrencar": no es reinicia
#                    de cop (per no fer flap), només es re-prova cada
#                    TEE_RETRY_SECS. Per defecte 20.
#   TEE_RETRY_SECS   Mode tee: quan una plataforma s'ha marcat com a avall,
#                    cada quants segons es reinicia el flux per retrobar-la
#                    (el directe fa un petit tall en cada intent, només mentre
#                    hi hagi una plataforma caiguda). Per defecte 300.
#   TWITCH_STREAM_KEY  (opcional; https://dashboard.twitch.tv > Configuració > Curs)
#   TWITCH_RTMP_URL    (default rtmp://live.twitch.tv/app)
#   YOUTUBE_STREAM_KEY (opcional; YouTube Studio > "Crea > Emet en directe" >
#                       clau d'emissió. Cal crear/iniciar un directe perquè la
#                       clau sigui vàlida: sense directe actiu l'ingest la
#                       rebutja o no mostra res al canal.)
#   YOUTUBE_RTMP_URL   (default rtmp://a.rtmp.youtube.com/live2; pots posar
#                       rtmps://a.rtmp.youtube.com/live2 per a la versió
#                       xifrada. YouTube també admet l'ingest B/C.D si `a`
#                       donés problemes.)
#   WIDTH/HEIGHT/FPS (default 1920/1080/30. IMPORTANT: el Chromium kiosk força
#                     sempre la finestra a 1920x1080; si la pantalla (Xvfb) és
#                     més petita, el contingut es RETALLA. Per tant l'escena es
#                     dissenya a 1080p i aquí es captura a 1080p.)
#   VIDEO_BITRATE    (default 5000k; max recomanat per Twitch ~6000k a 1080p.
#                     YouTube admet bitrates superiors, però amb la mateixa
#                     captura 1080p el 5000k és correcte.)
#   ENCODER          Codificador de vídeo: 'software' (libx264, CPU), 'vaapi'
#                    (h264_vaapi, GPU AMD/Intel via VAAPI) o 'auto' (prova
#                    vaapi i cau a libx264 si la GPU no està disponible).
#                    Per defecte: auto.
#                    Nota: amb DÜES plataformes hi ha dos processos ffmpeg
#                    codificant a la vegada; amb la GPU (VAAPI) no és
#                    problema, en mode software consumeix el doble de CPU.
#   VAAPI_DEVICE     Node DRM de render per a VAAPI (default /dev/dri/renderD128);
#                    cal que estigui muntat dins del container. Si el node no
#                    existeix o cap driver no funciona, es fa fallback a libx264.
#   LIBVA_DRIVER_NAME (opcional) Driver VA-API a forçar (p.ex. iHD o radeonsi).
#                    Si va buit, l'script els prova automàticament en ordre
#                    (auto, iHD, radeonsi) i fa servir el que funcioni — útil
#                    perquè en containers sense udev libva no auto-detecta.
#   VAAPI_QP         Qualitat de codificació amb la GPU (CQP: quantitzador
#                    constant 0-51; més baix = més nit i més bits). Per defecte
#                    26. NOTA: a Kaby Lake/Gen9 el driver iHD només accepta
#                    control de rate CQP (no VBR/CBR), per això en mode vaapi
#                    no es fan servir VIDEO_BITRATE/BUF.
set -euo pipefail

OVERLAY_URL="${OVERLAY_URL:-http://127.0.0.1:8080/overlay}"
TWITCH_URL="${TWITCH_RTMP_URL:-rtmp://live.twitch.tv/app}"
TWITCH_KEY="${TWITCH_STREAM_KEY:-}"
YOUTUBE_URL="${YOUTUBE_RTMP_URL:-rtmp://a.rtmp.youtube.com/live2}"
YOUTUBE_KEY="${YOUTUBE_STREAM_KEY:-}"
W="${WIDTH:-1920}"; H="${HEIGHT:-1080}"; FPS="${FPS:-30}"
VBR="${VIDEO_BITRATE:-5000k}"
BUF="${BUF:-10000k}"
ENCODER="${ENCODER:-auto}"
VAAPI_DEVICE="${VAAPI_DEVICE:-/dev/dri/renderD128}"
VAAPI_QP="${VAAPI_QP:-26}"
STREAM_MODE="$(printf '%s' "${STREAM_MODE:-tee}" | tr '[:upper:]' '[:lower:]')"
TEE_WARMUP_SECS="${TEE_WARMUP_SECS:-20}"
TEE_RETRY_SECS="${TEE_RETRY_SECS:-300}"
DISPLAY=:99
CHROME_PROFILE="${CHROME_PROFILE:-/tmp/clio-chrome}"

case "$STREAM_MODE" in
  tee|split) ;;
  *) echo "error: STREAM_MODE ha de ser tee o split (actual: $STREAM_MODE)" >&2; exit 1 ;;
esac

# Destinacions d'emissió: cada entrada "Nom|url_completa". Només les que tenen
# clau configurada. Cal com a mínim una per arrencar l'emissió.
DEST_C=()
[ -n "$TWITCH_KEY" ] && DEST_C+=("Twitch|${TWITCH_URL}/${TWITCH_KEY}")
[ -n "$YOUTUBE_KEY" ] && DEST_C+=("YouTube|${YOUTUBE_URL}/${YOUTUBE_KEY}")
if [ "${#DEST_C[@]}" -eq 0 ]; then
  echo "error: cal com a mínim TWITCH_STREAM_KEY o YOUTUBE_STREAM_KEY (o totes dues)" >&2
  exit 1
fi
case "$OVERLAY_URL" in
  http://*|https://*) ;;
  *) echo "error: OVERLAY_URL ha de ser http(s):// (actual: $OVERLAY_URL)" >&2; exit 1 ;;
esac

# Amaga la clau (últim segment del camí, p.ex. /app/CLAU) a TOTS els missatges
# de log: la URL completa només es passa a ffmpeg com a argument.
mask_rtmp() {
  printf '%s\n' "$1" | sed -E 's#^(.*/)[^/]+$#\1***#'
}
# Variant per a una LÍNIA sencera (la spec tee o una línia crua de ffmpeg) amb
# possiblement MÉS d'una URL: emmascara la clau (últim segment del camí, després
# de qualsevol nombre de directoris) de totes elles, fins a ' | o espai.
mask_urls() {
  printf '%s\n' "$1" | sed -E "s#(rtmps?://[^/ ]+(/[^/ ]+)*/)[^/'| ]+#\1***#g"
}

# Pids dels subshells (un bucle d'emissió per destí + watchdog) per aturar-los
# tots a la sortida; cada subshell es mata el seu propi ffmpeg dins del seu trap.
JOBS_PID=()
XPID=""
CLEANED=0
cleanup() {
  [ "$CLEANED" = 1 ] && return
  CLEANED=1
  local p
  for p in "${JOBS_PID[@]:-}"; do kill "$p" 2>/dev/null || true; done
  wait 2>/dev/null || true
  [ -n "$XPID" ] && kill "$XPID" 2>/dev/null || true
  sleep 1
}
trap cleanup EXIT INT TERM

echo "Destinacions d'emissió ($([ "$STREAM_MODE" = tee ] && echo "1 sola codificació via tee" || echo "1 procés ffmpeg per plataforma")):"
for entry in "${DEST_C[@]}"; do
  name="${entry%%|*}"
  echo "  - $name: $(mask_rtmp "${entry#*|}")"
done

# Espera que el servidor d'overlay respongui (fins 30s).
for i in $(seq 1 30); do
  if curl -fsS -o /dev/null "$OVERLAY_URL" 2>/dev/null; then break; fi
  [ "$i" = 30 ] && { echo "error: l'overlay no respon a $OVERLAY_URL" >&2; exit 1; }
  sleep 1
done

# Display virtual a la mida de l'escena (cal que coincideixi amb el que el
# kiosk renderitza, és a dir 1920x1080 —vegeu nota de WIDTH/HEIGHT—).
Xvfb "$DISPLAY" -screen 0 "${W}x${H}x24" -nolisten tcp &
XPID=$!
sleep 1

start_chromium() {
  # Perfil FRESC a cada arrencada + flags i preferències perquè la finestra
  # kiosk mostri EXACTAMENT l'overlay, una sola pestanya, sense cap element de
  # Chrome (traducció, "fer Chrome el navegador per defecte", infobars, etc.).
  rm -rf "$CHROME_PROFILE"
  mkdir -p "$CHROME_PROFILE/Default"
  cat > "$CHROME_PROFILE/Default/Preferences" <<'PREF'
{
  "browser": {
    "check_default_browser": false,
    "suppress_default_browser_prompt": true
  },
  "profile": { "exit_type": "Normal" }
}
PREF
  DISPLAY=$DISPLAY chromium \
    --no-sandbox --disable-gpu --disable-dev-shm-usage \
    --no-first-run --no-default-browser-check --disable-session-crashed-bubble \
    --disable-infobars --password-store=basic --check-for-update-interval=315360000 \
    --disable-features=Translate \
    --autoplay-policy=no-user-gesture-required \
    --lang=ca \
    --user-data-dir="$CHROME_PROFILE" \
    --window-size="${W},${H}" --window-position=0,0 --force-device-scale-factor=1 \
    --kiosk "$OVERLAY_URL" &
  CPID=$!
}

# Àudio de l'escena -> PulseAudio. El Chromium hi envia la música de fons i la
# veu dels titulars; ffmpeg captura el *monitor* d'un sink nul ("clio_out").
# Sense pulseaudio (o sense monitor), l'emissió continua en silenci (anullsrc).
AUDIO_DISABLED=1
start_audio() {
  command -v pulseaudio >/dev/null 2>&1 || { echo "avís: sense pulseaudio; emissió sense àudio." >&2; return 0; }
  pulseaudio --daemonize=yes --exit-idle-time=-1 --disallow-exit \
    --load="module-null-sink sink_name=clio_out sink_properties=device.description=ClioOut" \
    --load="module-always-sink" 2>/dev/null || true
  sleep 1
  if command -v pactl >/dev/null 2>&1; then
    pactl set-default-sink clio_out 2>/dev/null || true
    pactl set-sink-volume clio_out 100% 2>/dev/null || true
    # Lliga els clients (Chromium) al daemon concret que acabem d'aixecar. Sense
    # això, la discovery de libpulse depèn de XDG_RUNTIME_DIR (~/.config/pulse),
    # i als containers és fàcil que el client miri un socket que no existeix.
    local srv
    srv=$(pactl info 2>/dev/null | awk -F: '/Server String/{gsub(/^ +| +$/,"",$2); print $2}') || true
    if [ -n "$srv" ]; then
      export PULSE_SERVER="$srv"
      echo "PULSE_SERVER=$srv"
    fi
  fi
  if pactl list short sources 2>/dev/null | grep -q 'clio_out.monitor'; then
    AUDIO_DISABLED=0
    echo "Àudio de l'escena actiu (PulseAudio -> clio_out.monitor)."
  else
    echo "avís: no trobo clio_out.monitor; emissió sense àudio." >&2
  fi
}

# Garanteix que la finestra kiosk cobreixi tota la pantalla (0,0 WxH). Sense
# això, segons el build de Chromium, el kiosk es pot obrir més estret i deixar
# una franja/enciab negra al costat dret de la captura.
apply_window_geometry() {
  local WID=""
  for i in $(seq 1 30); do
    WID=$(DISPLAY=$DISPLAY xdotool search --sync --onlyvisible --name "" 2>/dev/null | head -1)
    [ -n "$WID" ] && break
    sleep 1
  done
  if [ -n "$WID" ]; then
    DISPLAY=$DISPLAY xdotool windowsize "$WID" "$W" "$H" 2>/dev/null
    DISPLAY=$DISPLAY xdotool windowmove "$WID" 0 0 2>/dev/null
    sleep 1
    echo "Finestra Chromium redimensionada a ${W}x${H} (id=$WID)"
  else
    echo "avís: no s'ha trobat la finestra de Chromium per redimensionar"
  fi
}
# IMPORTANT: PulseAudio ha de ser amunt ABANS del Chromium. El Chromium obre el
# PCM "default" en arrencar el servei d'àudio; la redirecció ALSA->Pulse de
# Debian (`libasound2-plugins`) s'avalua UNA sola vegada per procés, així que si
# el daemon encara no hi és quan Chromium fa el primer open, es queda amb
# "Unknown PCM default" i l'emissió va en silenci per sempre (fins a reiniciar).
start_audio
start_chromium
apply_window_geometry
sleep 2

# Prova ràpida (1 frame, sense escriure cap fitxer) de codificar H.264 via
# VAAPI amb UN driver concret; si cap driver no funciona, per a 24/7 és molt
# millor emetre amb libx264 que morir al primer frame.
# Driver "" = "deixa que libva decideixi"; en containers sense udev això sol
# fallar i cal provar-los un a un (vegeu `choose_encoder`).
VA_DRIVER=""
try_driver() {
  local d="$1"
  local ok
  if [ -n "$d" ]; then
    if ! ok=$(LIBVA_DRIVER_NAME="$d" ffmpeg -hide_banner -loglevel error \
          -vaapi_device "$VAAPI_DEVICE" \
          -f lavfi -i "testsrc2=size=320x180:rate=1" -frames:v 1 \
          -vf "format=nv12,hwupload" -c:v h264_vaapi -f null - 2>&1); then
      return 1
    fi
  else
    # "" = auto detect; cal treure la variable, no deixar-la buida.
    if ! ok=$(env -u LIBVA_DRIVER_NAME ffmpeg -hide_banner -loglevel error \
          -vaapi_device "$VAAPI_DEVICE" \
          -f lavfi -i "testsrc2=size=320x180:rate=1" -frames:v 1 \
          -vf "format=nv12,hwupload" -c:v h264_vaapi -f null - 2>&1); then
      return 1
    fi
  fi
  [ -z "$ok" ] || return 1
  VA_DRIVER="$d"
  return 0
}

# Triem codificador i driver VA-API: 'software' -> libx264; 'vaapi' ->
# h264_vaapi; 'auto' -> vaapi si la GPU hi és, sinó libx264. Fallback sempre
# amb avís, per no perdre el directe. Ordre de drivers provats: el demanat per
# l'usuari (LIBVA_DRIVER_NAME), auto, Intel iHD, AMD radeonsi.
# La tria es fa UNA vegada (abans de llançar els bucles) i és compartida per
# TOTS els processos ffmpeg de les diferents plataformes.
choose_encoder() {
  [ -e "$VAAPI_DEVICE" ] || {
    echo "avís: no existeix $VAAPI_DEVICE; sense acceleració de vídeo." >&2
    VENC=software
    return
  }
  local forbida=0
  case "$(printf '%s' "$ENCODER" | tr '[:upper:]' '[:lower:]')" in
    software) VENC=software; return ;;
    vaapi) forbida=1 ;;
    auto|*) forbida=0 ;;
  esac
  if [ -n "${LIBVA_DRIVER_NAME:-}" ]; then
    try_driver "$LIBVA_DRIVER_NAME" && VENC=vaapi || VENC=software
  elif try_driver "" || try_driver iHD || try_driver radeonsi; then
    VENC=vaapi
  else
    VENC=software
  fi
  if [ "$VENC" = software ]; then
    if [ "$forbida" = 1 ]; then
      echo "Avís: cap driver VA-API no ha pogut inicialitzar h264_vaapi a $VAAPI_DEVICE; caic a libx264." >&2
    else
      echo "Sense GPU usable: faig servir libx264 (CPU)."
    fi
  else
    echo "GPU detectada: h264_vaapi ($VAAPI_DEVICE, driver ${VA_DRIVER:-auto})."
    # Fica el driver que vam validar al probe (o treu-lo si era "auto") perquè
    # tots els processos ffmpeg facin servir exactament el mateix.
    if [ -n "$VA_DRIVER" ]; then
      export LIBVA_DRIVER_NAME="$VA_DRIVER"
    else
      unset LIBVA_DRIVER_NAME
    fi
  fi
}
VENC=""
choose_encoder

# Construeix els arguments COMPUNTS de ffmpeg (entrada de vídeo, àudio i
# codificació) a ARGV. El destí RTMP l'afegeix cada bucle d'emissió.
ARGV=()
build_ffmpeg_args() {
  ARGV=(
    -hide_banner -loglevel warning
    -f x11grab -video_size "${W}x${H}" -framerate "$FPS" -draw_mouse 0 -i "$DISPLAY"
  )
  # Àudio real (monitor de PulseAudio) si hi és; sinó silenci (anullsrc). Amb
  # aquest ordre ffmpeg obre l'àudio abans del vídeo; és indiferent pel flux.
  if [ "$AUDIO_DISABLED" = 0 ]; then
    ARGV+=(-f pulse -i clio_out.monitor)
  else
    ARGV+=(-f lavfi -i "anullsrc=channel_layout=stereo:sample_rate=44100")
  fi
  # IMPORTANT: el muxer `tee` (mode STREAM_MODE=tee) NO fa auto-mapping: sense
  # `-map` explícit, ffmpeg mor a l'obertura amb "Output file does not contain
  # any stream" (no es mapeja cap stream a la sortida). Amb el map explícit
  # (vídeo = entrada 0, àudio = entrada 1, que és SEMPRE com es construeixen
  # aquí) funciona tant amb tee com amb el flv directe del mode split.
  ARGV+=(-map 0:v -map 1:a)
  if [ "$VENC" = vaapi ]; then
    # Codificació a la GPU (AMD/Intel via VAAPI): allibera la CPU que abans
    # gastava libx264. La conversió a nv12 + hwupload la fa ffmpeg a la CPU
    # (barata); la compressió H.264 la fa el còdec de maquinari de la iGPU.
    # Control de rate CQP (quantitzador constant): a Gen9/Kaby Lake el driver
    # iHD només suporta CQP; si es passés bitrate (b:v/maxrate/bufsize),
    # l'encoder no s'obriria. La qualitat es regula amb VAAPI_QP.
    ARGV+=(
      -vaapi_device "$VAAPI_DEVICE"
      -vf "format=nv12,hwupload"
      -c:v h264_vaapi -rc_mode CQP -qp "$VAAPI_QP" -bf 0
      -g "$((FPS*2))" -keyint_min "$((FPS*2))"
    )
    echo "  codificador: h264_vaapi ($VAAPI_DEVICE, driver ${VA_DRIVER:-auto})"
  else
    ARGV+=(
      -c:v libx264 -preset veryfast -b:v "$VBR" -maxrate "$VBR" -bufsize "$BUF"
      -pix_fmt yuv420p -g "$((FPS*2))" -keyint_min "$((FPS*2))" -sc_threshold 0
    )
    echo "  codificador: libx264 (software)"
  fi
  ARGV+=(-c:a aac -b:a 128k -ar 44100 -ac 2)
}

# Bucle d'emissió PER DESTÍ (mode `split`): reinicia només aquest ffmpeg si cau
# la connexió. S'executa en un subshell amb el seu propi trap: en rebre TERM/INT
# marca `stop` (per no tornar a arrencar) i mata el seu ffmpeg; així cada
# plataforma s'atura sola sense tocar les altres.
stream_to() {
  local name="$1" dest="$2"
  (
    ffpid=""
    stop=0
    trap 'stop=1; [ -n "${ffpid:-}" ] && kill "$ffpid" 2>/dev/null || true' TERM INT
    while [ "$stop" -eq 0 ]; do
      echo "[$name] Iniciant emissió -> $(mask_rtmp "$dest")"
      build_ffmpeg_args
      ARGV+=(-f flv "$dest")
      ffmpeg "${ARGV[@]}" &
      ffpid=$!
      if ! wait "$ffpid"; then
        ffpid=""
        # Si ens estan aturant, no escrius un fals "ha caigut".
        [ "$stop" -eq 0 ] && echo "[$name] ffmpeg ha caigut; reiniciant en 3s..."
      else
        ffpid=""
      fi
      [ "$stop" -eq 0 ] && sleep 3
    done
  ) &
  JOBS_PID+=("$!")
}

# Watcher del mode `tee`: vigila el log de l'únic ffmpeg i decideix quan cal
# reiniciar-lo per re-conectar una plataforma caiguda, sense re-codificar ni
# fer flap:
#   - un "slave failed" PASSAT el warmup (caiguda a mig directe) -> mata ffmpeg
#     ara mateix (el bucle el rellança en 3 s i es re-conecta);
#   - un "slave failed" DINS el warmup (la plataforma ja era avall en arrencar)
#     -> només marca un downflag; el retryer la tornarà a provar cada
#     TEE_RETRY_SECS reiniciant el flux, sense picar cada pocs segons.
# Els watchers es destrueixen sols quan ffmpeg mor (tail --pid i /proc).
start_tee_watchdog() {
  local log="$1" pid="$2" flag="$3"
  (
    local start
    start=$(date +%s)
    tail -n +1 -F --pid="$pid" "$log" 2>/dev/null | while IFS= read -r line; do
      case "$line" in
        *"Slave muxer #"*"failed"*)
          if [ $(( $(date +%s) - start )) -gt "$TEE_WARMUP_SECS" ]; then
            echo "[tee] $(mask_urls "$line")" >&2
            echo "[tee] plataforma caiguda: reinicio per re-conectar." >&2
            kill "$pid" 2>/dev/null
          else
            touch "$flag"
            echo "[tee] plataforma avall en arrencar; es re-provarà cada ${TEE_RETRY_SECS}s." >&2
          fi
          ;;
      esac
    done
  ) &
  (
    local start
    start=$(date +%s)
    while [ -e "/proc/$pid" ]; do
      sleep "$TEE_RETRY_SECS"
      [ -e "/proc/$pid" ] || break
      if [ -e "$flag" ] && [ $(( $(date +%s) - start )) -gt "$TEE_WARMUP_SECS" ]; then
        echo "[tee] re-intent programat per re-enganxar la plataforma avall." >&2
        kill "$pid" 2>/dev/null
      fi
    done
  ) &
}

# Bucle d'emissió ÚNICA (mode `tee`): un ffmpeg codifica una vegada i el muxer
# `tee` entrega EL MATEIX flux a totes les plataformes. El watcher
# (start_tee_watchdog) s'encarrega de la reconnexió de les que caiguin.
stream_tee() {
  (
    local spec="" i url
    spec=""
    for i in "${!DEST_C[@]}"; do
      url="${DEST_C[$i]#*|}"
      spec+="[f=flv:onfail=ignore]${url}|"
    done
    spec="${spec%|}"
    local TEE_LOG TEE_FLAG
    TEE_LOG="${TMPDIR:-/tmp}/clio-tee.log"
    TEE_FLAG="${TMPDIR:-/tmp}/clio-tee.down"
    ffpid=""
    stop=0
    trap 'stop=1; [ -n "${ffpid:-}" ] && kill "$ffpid" 2>/dev/null || true' TERM INT
    while [ "$stop" -eq 0 ]; do
      echo "[tee] Iniciant emissió única (una codificació) -> $(mask_urls "$spec")"
      build_ffmpeg_args
      ARGV+=(-f tee "$spec")
      rm -f "$TEE_FLAG"
      : > "$TEE_LOG"
      ffmpeg "${ARGV[@]}" > "$TEE_LOG" 2>&1 &
      ffpid=$!
      start_tee_watchdog "$TEE_LOG" "$ffpid" "$TEE_FLAG"
      if ! wait "$ffpid"; then
        ffpid=""
        [ "$stop" -eq 0 ] && echo "[tee] ffmpeg ha caigut; reiniciant en 3s..." >&2
      else
        ffpid=""
      fi
      [ "$stop" -eq 0 ] && sleep 3
    done
  ) &
  JOBS_PID+=("$!")
}

# Watchdog del Chromium: si mor, el torna a aixecar (independent dels bucles
# d'emissió, perquè tots capturen la mateixa finestra i no es poden trepitjar).
# El mateix patró `stop`: a la sortida es mata l'últim Chromium i s'acaba.
#
# Comprovem per /proc que el pid és REALMENT el nostre Chromium (argument
# --kiosk): un PID sol se reutilitzar quan un procés mor, i `kill -0` podria
# dir-nos "vivent" per accident i fer-nos creure que el Chromium és amunt.
is_our_chromium() {
  [ -n "${CPID:-}" ] || return 1
  kill -0 "$CPID" 2>/dev/null || return 1
  tr '\0' ' ' < "/proc/$CPID/cmdline" 2>/dev/null | grep -q -- '--kiosk'
}
watchdog_chromium() {
  (
    stop=0
    trap 'stop=1; [ -n "${CPID:-}" ] && kill "$CPID" 2>/dev/null || true' TERM INT
    while [ "$stop" -eq 0 ]; do
      sleep 5
      [ "$stop" -eq 0 ] || continue
      if ! is_our_chromium; then
        echo "Chromium ha mort; reiniciant."
        start_chromium
        apply_window_geometry
      fi
    done
  ) &
  JOBS_PID+=("$!")
}

# Arrenca l'emissió segons el mode: `tee` = un sol ffmpeg / `split` = un per
# plataforma. En tots dos casos el watchdog del Chromium corre a part.
case "$STREAM_MODE" in
  tee)   stream_tee ;;
  split) for entry in "${DEST_C[@]}"; do stream_to "${entry%%|*}" "${entry#*|}"; done ;;
esac
watchdog_chromium

# Espera infinita: els traps fan la neteja (mata subshells -> cada un el seu
# ffmpeg, més el Xvfb).
wait

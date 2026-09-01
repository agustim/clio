#!/usr/bin/env bash
# Clio Overlay -> Twitch (sense OBS).
#
# Cadenes de 24/7: Xvfb (display virtual) + Chromium (escena /overlay) capturats
# per ffmpeg (x11grab) i pujats per RTMP a Twitch. Si la connexió cau, ffmpeg es
# reinicia sol; Chromium també si ha mort.
#
# Config (variables d'entorn):
#   OVERLAY_URL      (default http://127.0.0.1:8080/overlay)
#   CHROME_PROFILE   (perfil de Chromium dedicat; es buida a cada arrencada.
#                     Evita pestanyes bufades i finestres de Chrome: "posar com
#                     a navegador per defecte", traducció, restauració de sessió,
#                     "This space intentionally blank…", etc.)
#   TWITCH_STREAM_KEY (obligatòria; https://dashboard.twitch.tv > Configuració > Curs)
#   TWITCH_RTMP_URL  (default rtmp://live.twitch.tv/app)
#   WIDTH/HEIGHT/FPS (default 1920/1080/30. IMPORTANT: el Chromium kiosk força
#                     sempre la finestra a 1920x1080; si la pantalla (Xvfb) és
#                     més petita, el contingut es RETALLA. Per tant l'escena es
#                     dissenya a 1080p i aquí es captura a 1080p.)
#   VIDEO_BITRATE    (default 5000k; max recomanat per Twitch ~6000k a 1080p)
#   ENCODER          Codificador de vídeo: 'software' (libx264, CPU), 'vaapi'
#                    (h264_vaapi, GPU AMD/Intel via VAAPI) o 'auto' (prova
#                    vaapi i cau a libx264 si la GPU no està disponible).
#                    Per defecte: auto.
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
RTMP_URL="${TWITCH_RTMP_URL:-rtmp://live.twitch.tv/app}"
KEY="${TWITCH_STREAM_KEY:-}"
W="${WIDTH:-1920}"; H="${HEIGHT:-1080}"; FPS="${FPS:-30}"
VBR="${VIDEO_BITRATE:-5000k}"
BUF="${BUF:-10000k}"
ENCODER="${ENCODER:-auto}"
VAAPI_DEVICE="${VAAPI_DEVICE:-/dev/dri/renderD128}"
VAAPI_QP="${VAAPI_QP:-26}"
DISPLAY=:99
CHROME_PROFILE="${CHROME_PROFILE:-/tmp/clio-chrome}"

[ -n "$KEY" ] || { echo "error: cal TWITCH_STREAM_KEY" >&2; exit 1; }
case "$OVERLAY_URL" in
  http://*|https://*) ;;
  *) echo "error: OVERLAY_URL ha de ser http(s):// (actual: $OVERLAY_URL)" >&2; exit 1 ;;
esac

cleanup() {
  [ -n "${FFPID:-}" ] && kill "$FFPID" 2>/dev/null || true
  [ -n "${CPID:-}" ] && kill "$CPID" 2>/dev/null || true
  [ -n "${XPID:-}" ] && kill "$XPID" 2>/dev/null || true
  sleep 1
}
trap cleanup EXIT INT TERM

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
start_chromium
apply_window_geometry
start_audio
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
  fi
}
VENC=""
choose_encoder

# Bucle d'emissió: reinicia ffmpeg si cau la connexió.
while true; do
  echo "Iniciant emissió -> ${RTMP_URL}/${KEY}"
  FFARGS=(
    -hide_banner -loglevel warning
    -f x11grab -video_size "${W}x${H}" -framerate "$FPS" -draw_mouse 0 -i "$DISPLAY"
  )
  # Àudio real (monitor de PulseAudio) si hi és; sinó silenci (anullsrc). Amb
  # aquest ordre ffmpeg obre l'àudio abans del vídeo; és indiferent pel flux.
  if [ "$AUDIO_DISABLED" = 0 ]; then
    FFARGS+=(-f pulse -i clio_out.monitor)
  else
    FFARGS+=(-f lavfi -i "anullsrc=channel_layout=stereo:sample_rate=44100")
  fi
  if [ "$VENC" = vaapi ]; then
    # Codificació a la GPU (AMD/Intel via VAAPI): allibera la CPU que abans
    # gastava libx264. La conversió a nv12 + hwupload la fa ffmpeg a la CPU
    # (barata); la compressió H.264 la fa el còdec de maquinari de la iGPU.
    # Control de rate CQP (quantitzador constant): a Gen9/Kaby Lake el driver
    # iHD només suporta CQP; si es passés bitrate (b:v/maxrate/bufsize),
    # l'encoder no s'obriria. La qualitat es regula amb VAAPI_QP.
    FFARGS+=(
      -vaapi_device "$VAAPI_DEVICE"
      -vf "format=nv12,hwupload"
      -c:v h264_vaapi -rc_mode CQP -qp "$VAAPI_QP" -bf 0
      -g "$((FPS*2))" -keyint_min "$((FPS*2))"
    )
    # Fica el driver que vam validar al probe (o treu-lo si era "auto"), per
    # què l'emissió real faci servir exactament el mateix.
    if [ -n "$VA_DRIVER" ]; then
      export LIBVA_DRIVER_NAME="$VA_DRIVER"
    else
      unset LIBVA_DRIVER_NAME
    fi
    echo "  codificador: h264_vaapi ($VAAPI_DEVICE, driver ${VA_DRIVER:-auto})"
  else
    FFARGS+=(
      -c:v libx264 -preset veryfast -b:v "$VBR" -maxrate "$VBR" -bufsize "$BUF"
      -pix_fmt yuv420p -g "$((FPS*2))" -keyint_min "$((FPS*2))" -sc_threshold 0
    )
    echo "  codificador: libx264 (software)"
  fi
  FFARGS+=(-c:a aac -b:a 128k -ar 44100 -ac 2 -f flv "${RTMP_URL}/${KEY}")
  ffmpeg "${FFARGS[@]}" &
  FFPID=$!
  wait $FFPID || echo "ffmpeg ha caigut; reiniciant en 3s..."
  FFPID=
  if ! kill -0 "$CPID" 2>/dev/null; then
    echo "Chromium ha mort; reiniciant."
    start_chromium
  fi
  sleep 3
done

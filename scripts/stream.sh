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
set -euo pipefail

OVERLAY_URL="${OVERLAY_URL:-http://127.0.0.1:8080/overlay}"
RTMP_URL="${TWITCH_RTMP_URL:-rtmp://live.twitch.tv/app}"
KEY="${TWITCH_STREAM_KEY:-}"
W="${WIDTH:-1920}"; H="${HEIGHT:-1080}"; FPS="${FPS:-30}"
VBR="${VIDEO_BITRATE:-5000k}"
BUF="${BUF:-10000k}"
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
    --lang=ca \
    --user-data-dir="$CHROME_PROFILE" \
    --window-size="${W},${H}" --window-position=0,0 --force-device-scale-factor=1 \
    --kiosk "$OVERLAY_URL" &
  CPID=$!
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
sleep 2

# Bucle d'emissió: reinicia ffmpeg si cau la connexió.
while true; do
  echo "Iniciant emissió -> ${RTMP_URL}/${KEY}"
  ffmpeg -hide_banner -loglevel warning \
    -f x11grab -video_size "${W}x${H}" -framerate "$FPS" -draw_mouse 0 -i "$DISPLAY" \
    -f lavfi -i "anullsrc=channel_layout=stereo:sample_rate=44100" \
    -c:v libx264 -preset veryfast -b:v "$VBR" -maxrate "$VBR" -bufsize "$BUF" \
    -pix_fmt yuv420p -g "$((FPS*2))" -keyint_min "$((FPS*2))" -sc_threshold 0 \
    -c:a aac -b:a 128k -ar 44100 -ac 2 \
    -f flv "${RTMP_URL}/${KEY}" &
  FFPID=$!
  wait $FFPID || echo "ffmpeg ha caigut; reiniciant en 3s..."
  FFPID=
  if ! kill -0 "$CPID" 2>/dev/null; then
    echo "Chromium ha mort; reiniciant."
    start_chromium
  fi
  sleep 3
done

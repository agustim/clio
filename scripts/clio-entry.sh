#!/usr/bin/env bash
# Punt d'entrada del container "tot en un" (target `stream` del Dockerfile).
# Tria què executar:
#   CLIO_MODE=stream            -> emissió headless cap a Twitch (stream.sh)
#   CLIO_MODE=serve (o buit)    -> API + web (com la imatge lleugera `app`)
#   arguments directes          -> qualsevol subordre de la CLI (images, reindex, add...)
#                                p.ex. `docker run clio images --limit 5`
set -euo pipefail

MODE="${CLIO_MODE:-}"

if [ "$MODE" = "stream" ]; then
  exec /usr/local/bin/stream.sh
fi

exec linkanalyzer "${@:-serve}"

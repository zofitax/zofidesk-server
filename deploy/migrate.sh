#!/usr/bin/env bash
# Moves a stock rustdesk-server install on this host to the ZofiDesk server whose
# docker-compose.yml, Caddyfile and .env sit next to this script.
#
#   sudo ./migrate.sh <old data dir> <old container>...    e.g. sudo ./migrate.sh /root/rustdesk/data hbbs hbbr
#   sudo ./migrate.sh --rollback <old container>...        stops ZofiDesk and starts the old containers again
#
# The old containers are stopped, not removed, and their data is only read (and backed up), so
# rolling back is always possible.

set -euo pipefail
SELF="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
cd "$(dirname "$SELF")"

# Public key built into the ZofiDesk clients: the migrated server must keep it.
EXPECTED_KEY="HUkjATXqS3KfsKm4yDzq1izuQr9nHibvoeBddy7LVJI="
ZOFIDESK_UID=10001

die() { echo "ERROR: $*" >&2; exit 1; }
step() { printf '\n==> %s\n' "$*"; }
confirm() {
  read -r -p "$1 [s/N] " answer
  case "$answer" in s|S) ;; *) die "Cancelado" ;; esac
}

if [ "${1:-}" = "--rollback" ]; then
  shift
  [ $# -gt 0 ] || die "Indica los contenedores antiguos, p. ej. hbbs hbbr"
  confirm "Parar ZofiDesk y volver a arrancar $*?"
  docker compose down
  docker update --restart=unless-stopped "$@" >/dev/null
  docker start "$@"
  echo "Vuelta atrás hecha. ./data queda intacto por si se reintenta la migración."
  exit 0
fi

[ $# -ge 2 ] || die "Uso: $0 <carpeta de datos antigua> <contenedor antiguo>..."
# Relative to where the script was started, not to this directory.
OLD_DATA=$(cd "$OLDPWD" && cd "$1" && pwd) || die "No existe la carpeta $1"
shift
OLD_CONTAINERS=("$@")

step "Comprobaciones previas"
[ "$(id -u)" = 0 ] || die "Ejecútalo con sudo"
for file in docker-compose.yml Caddyfile .env; do
  [ -f "$file" ] || die "Falta $file junto a este script"
done
set -a
. ./.env
set +a
: "${ZOFIDESK_IMAGE:?falta ZOFIDESK_IMAGE en .env}" "${ZOFIDESK_DOMAIN:?falta ZOFIDESK_DOMAIN en .env}"
[ -f "$OLD_DATA/id_ed25519" ] && [ -f "$OLD_DATA/id_ed25519.pub" ] \
  || die "No están id_ed25519 e id_ed25519.pub en $OLD_DATA"
[ "$(cat "$OLD_DATA/id_ed25519.pub")" = "$EXPECTED_KEY" ] \
  || die "La clave de $OLD_DATA no es la de los clientes ZofiDesk"
if [ -d data ] && [ -n "$(ls -A data)" ]; then
  die "./data ya existe y no está vacía: revísala o muévela antes de migrar"
fi
for container in "${OLD_CONTAINERS[@]}"; do
  docker inspect "$container" >/dev/null 2>&1 || die "No existe el contenedor $container"
done
if ss -ltn '( sport = :80 or sport = :443 )' | grep -q LISTEN; then
  die "Otro servicio usa el puerto 80 o 443; Caddy no podría arrancar"
fi
case "$ZOFIDESK_IMAGE" in
  *:latest) echo "Aviso: ZOFIDESK_IMAGE usa :latest; mejor una versión fija (p. ej. :0.1.0)" ;;
esac
if [ "${MUST_LOGIN:-N}" = "Y" ]; then
  confirm "MUST_LOGIN=Y: los técnicos sin cuenta dejarán de poder conectarse. ¿Seguro?"
fi

cat <<EOF

Plan:
  1. Descargar $ZOFIDESK_IMAGE
  2. Parar ${OLD_CONTAINERS[*]} (sin borrarlos) y desactivar su reinicio automático
  3. Copia de seguridad de $OLD_DATA
  4. Copiar la clave y db_v2.sqlite3 a ./data
  5. Arrancar ZofiDesk y Caddy, y comprobar clave, relay y API
El servicio estará parado desde el paso 2 hasta el 5 (normalmente menos de un minuto).
EOF
confirm "¿Continuar?"

step "1. Descargando la imagen"
docker compose pull

step "2. Parando los contenedores antiguos"
docker update --restart=no "${OLD_CONTAINERS[@]}" >/dev/null
docker stop "${OLD_CONTAINERS[@]}"

step "3. Copia de seguridad"
backup="backup-rustdesk-$(date +%Y%m%d-%H%M%S).tgz"
tar czf "$backup" -C "$OLD_DATA" .
chmod 600 "$backup"
echo "Guardada en $PWD/$backup (contiene la clave privada)"

step "4. Copiando datos"
mkdir -p data
cp -p "$OLD_DATA"/id_ed25519 "$OLD_DATA"/id_ed25519.pub data/
for file in "$OLD_DATA"/db_v2.sqlite3*; do
  [ -e "$file" ] && cp -p "$file" data/
done
chown -R "$ZOFIDESK_UID:$ZOFIDESK_UID" data
chmod 600 data/id_ed25519
docker run --rm "$ZOFIDESK_IMAGE" rustdesk-utils validatekeypair \
  "$(cat data/id_ed25519.pub)" "$(cat data/id_ed25519)" \
  || die "El par de claves no es válido. Vuelta atrás: sudo $SELF --rollback ${OLD_CONTAINERS[*]}"

step "5. Arrancando ZofiDesk"
docker compose up -d
ok=
for _ in $(seq 30); do
  logs=$(docker compose logs zofidesk 2>&1)
  if grep -q "Listening on tcp :21117" <<<"$logs" && grep -q "ZofiDesk API listening" <<<"$logs"; then
    ok=1
    break
  fi
  sleep 1
done
failed() {
  echo "ERROR: $*" >&2
  echo "Log: docker compose logs zofidesk" >&2
  echo "Vuelta atrás: sudo $SELF --rollback ${OLD_CONTAINERS[*]}" >&2
  exit 1
}
[ -n "$ok" ] || failed "ZofiDesk no arrancó el relay o la API en 30 s"
grep -q "Key: $EXPECTED_KEY" <<<"$logs" || failed "ZofiDesk arrancó con otra clave"
grep -q "API listening on 127.0.0.1:21114" <<<"$logs" || failed "La API no escucha solo en 127.0.0.1"
grep -E "MUST_LOGIN=" <<<"$logs" | tail -1

echo "Esperando el certificado HTTPS de $ZOFIDESK_DOMAIN..."
api_ok=
for _ in $(seq 24); do
  if [ "$(curl -fsS "https://$ZOFIDESK_DOMAIN/api/login-options" 2>/dev/null)" = "[]" ]; then
    api_ok=1
    break
  fi
  sleep 5
done
if [ -n "$api_ok" ]; then
  echo "API por HTTPS: OK"
else
  echo "Aviso: https://$ZOFIDESK_DOMAIN/api/login-options aún no responde. Revisa: docker compose logs caddy"
fi

cat <<EOF

Migración hecha. ZofiDesk ya atiende en los puertos de siempre.
  - Crear el primer usuario:  docker compose exec zofidesk zofidesk-admin user add <usuario> --admin
  - Vuelta atrás si hace falta: sudo $SELF --rollback ${OLD_CONTAINERS[*]}
  - Cuando todo vaya bien durante unos días se pueden borrar los contenedores antiguos:
      docker rm ${OLD_CONTAINERS[*]}
EOF

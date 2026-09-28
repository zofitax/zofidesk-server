#!/usr/bin/env bash
# Collects what the ZofiDesk migration needs to know about this VPS. It only reads: it changes
# nothing and never prints private keys or container environments.
# Usage: sudo ./vps-check.sh [domain]    (default domain: zigno.zofi.tax)

set -u
DOMAIN="${1:-zigno.zofi.tax}"
# Public key built into the ZofiDesk clients.
EXPECTED_KEY="HUkjATXqS3KfsKm4yDzq1izuQr9nHibvoeBddy7LVJI="

section() { printf '\n=== %s\n' "$1"; }

section "Sistema"
uname -srm
[ -r /etc/os-release ] && . /etc/os-release && echo "${PRETTY_NAME:-}"
df -h / | tail -1

section "Docker"
docker version --format 'Docker {{.Server.Version}}' 2>&1
docker compose version 2>&1

section "Contenedores"
docker ps -a --format 'table {{.Names}}\t{{.Image}}\t{{.Status}}\t{{.Ports}}'

section "Contenedores de RustDesk"
for name in $(docker ps -a --format '{{.Names}}'); do
  image=$(docker inspect -f '{{.Config.Image}}' "$name")
  case "$name $image" in
    *rustdesk*|*hbbs*|*hbbr*) ;;
    *) continue ;;
  esac
  echo "--- $name ($image)"
  docker inspect -f 'Comando: {{.Path}} {{join .Args " "}}
Red: {{.HostConfig.NetworkMode}}
Reinicio: {{.HostConfig.RestartPolicy.Name}}
Compose: {{index .Config.Labels "com.docker.compose.project.working_dir"}}' "$name"
  # Only the variables that shape the setup; KEY_PRIV and the like are never printed.
  docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' "$name" \
    | grep -E '^(RELAY|ENCRYPTED_ONLY|KEY_PUB|PORT)=' | sed 's/^/Env: /'
  docker inspect -f '{{range .Mounts}}{{.Source}} -> {{.Destination}}{{println}}{{end}}' "$name" \
    | while read -r src _ dst; do
        [ -n "$src" ] || continue
        echo "Volumen: $src -> $dst"
        if [ -f "$src/id_ed25519.pub" ]; then
          key=$(cat "$src/id_ed25519.pub")
          if [ "$key" = "$EXPECTED_KEY" ]; then verdict="coincide con los clientes"; else verdict="NO coincide con los clientes"; fi
          echo "  Clave pública: $key ($verdict)"
          [ -f "$src/id_ed25519" ] && echo "  Clave privada: presente" || echo "  Clave privada: FALTA"
        fi
        ls -la "$src" 2>/dev/null | sed 's/^/  /'
      done
done

section "Puertos en uso (80, 443, 21114-21119, 31114-31119)"
ss -ltnup 2>/dev/null | awk 'NR==1 || $5 ~ /:(80|443|2111[4-9]|3111[4-9])$/'

section "Firewall"
if command -v ufw >/dev/null && ufw status | grep -q active; then
  ufw status verbose
elif command -v firewall-cmd >/dev/null && firewall-cmd --state >/dev/null 2>&1; then
  firewall-cmd --list-all
else
  echo "Ni ufw ni firewalld activos. Reglas INPUT de iptables:"
  iptables -S INPUT 2>&1 | head -30
fi

section "DNS de $DOMAIN"
getent ahostsv4 "$DOMAIN" | awk '{print $1}' | sort -u | sed 's/^/Resuelve a: /'
ip -4 -o addr show scope global | awk '{print "IP local: " $4}'

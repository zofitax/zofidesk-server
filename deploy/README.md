# Despliegue del servidor ZofiDesk

Un solo contenedor `zofidesk` ejecuta hbbs con la API y el relay integrado (`EMBEDDED_RELAY=Y`);
ya no hay contenedor `hbbr`. Caddy pone HTTPS delante de la API. Ambos usan la red del host para
que hbbs vea las direcciones reales de los clientes.

| Puerto | Uso | Abrir en el firewall |
|---|---|---|
| 80, 443/tcp | Caddy (HTTPS de la API y certificado) | Sí |
| 21114/tcp | API, solo en 127.0.0.1 (`API_BIND`) | **No** |
| 21115/tcp | Prueba de NAT | Sí |
| 21116/tcp+udp | Rendezvous (ID) | Sí |
| 21117/tcp | Relay | Sí |
| 21118, 21119/tcp | WebSocket (cliente web) | Solo si se usa |

## Imagen

El workflow `zofi-docker` publica `ghcr.io/zofitax/zofidesk-server`:

- una etiqueta `zofi-vX.Y.Z` publica `X.Y.Z` y `latest`;
- una etiqueta de prueba `zofi-vX.Y.Z-rc1` publica solo `X.Y.Z-rc1` (sirve para probar una rama
  sin tocar `latest`);
- lanzarlo a mano desde Actions (solo cuando el workflow ya está en `master`) publica solo
  `sha-<commit>`.

Todas publican además `sha-<commit>`. Hacer push de una rama no ejecuta ningún workflow.

Si el paquete es privado, en el VPS hace falta `docker login ghcr.io` con un token que tenga `read:packages`.

## Primera instalación (migración desde rustdesk-server)

1. **Copia de seguridad** de la carpeta de datos actual (`./data` del compose anterior). Tiene
   que contener `id_ed25519` e `id_ed25519.pub`: es la clave que ya tienen los clientes instalados.
2. En el VPS, crear p. ej. `/opt/zofidesk` con `docker-compose.yml`, `Caddyfile` y
   `.env` (copiado de `.env.example`).
3. Copiar los datos antiguos a `/opt/zofidesk/data` y darle la carpeta al usuario del contenedor:

   ```sh
   sudo chown -R 10001:10001 /opt/zofidesk/data
   ```

4. Parar los contenedores antiguos `hbbs` y `hbbr` (usan los mismos puertos) y arrancar:

   ```sh
   docker compose pull
   docker compose up -d
   docker compose logs zofidesk
   ```

   En el log debe aparecer la **misma** `Key:` que antes, `Listening on tcp :21117` (relay) y
   `ZofiDesk API listening on 127.0.0.1:21114`. Si la clave es otra, no se copiaron bien los
   ficheros `id_ed25519*`: parar y revisar antes de que se conecten los clientes.
5. Crear el primer usuario (la contraseña se pide por teclado):

   ```sh
   docker compose exec zofidesk zofidesk-admin user add luis --admin
   ```

6. Comprobar la API: `curl https://zigno.zofi.tax/api/login-options` debe responder `[]`.

Con `MUST_LOGIN=N` el servidor se comporta como el original: los clientes instalados siguen
funcionando sin cuenta.

## Activar el control de acceso

Cuando los técnicos tengan cuenta y un cliente ZofiDesk con `api-server=https://zigno.zofi.tax`:

1. Poner `MUST_LOGIN=Y` en `.env`.
2. `docker compose up -d` (recrea el contenedor).
3. En el log debe aparecer `MUST_LOGIN=Y`.

Desde ese momento solo un usuario con sesión iniciada puede conectarse a otros equipos o usar el
relay. Los equipos controlados no necesitan cuenta.

## Volver atrás

Basta con `docker compose down` y arrancar de nuevo los contenedores antiguos con la misma carpeta
de datos. `db_v2.sqlite3` y las claves no cambian de formato; el original ignora `zofidesk.sqlite3`.
Si la imagen antigua corre como root, los permisos cambiados en el paso 3 no le afectan.

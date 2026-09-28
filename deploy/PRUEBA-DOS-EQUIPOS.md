# Prueba del control de acceso entre dos equipos

Objetivo: comprobar con clientes ZofiDesk reales que, con `MUST_LOGIN=Y`, solo un usuario con
sesión puede conectarse a otro equipo, por conexión directa y por relay. También hay que comprobar
que con `MUST_LOGIN=N` todo sigue funcionando como hoy.

- **Equipo A (técnico):** es el que controla e inicia sesión.
- **Equipo B (controlado):** hace de PC de cliente y nunca inicia sesión.

La prueba usa una **instancia aparte en el VPS, en los puertos 3111x**. El servidor de producción
(2111x) no se toca, y los clientes instalados no se enteran.

## 1. Servidor de prueba en el VPS

Usa una copia de la clave de producción, así los clientes de prueba no tienen que cambiar la clave.

```sh
sudo mkdir -p /opt/zofidesk-test/data
sudo cp <carpeta de datos de producción>/id_ed25519 <carpeta de datos de producción>/id_ed25519.pub /opt/zofidesk-test/data/
sudo chown -R 10001:10001 /opt/zofidesk-test/data

cat > /opt/zofidesk-test/test.env <<'EOF'
PORT=31116
RELAY_SERVERS=zigno.zofi.tax:31117
EMBEDDED_RELAY=Y
MUST_LOGIN=N
ALWAYS_USE_RELAY=N
RUST_LOG=info,hbbs::zofi=debug
EOF
```

Script para (re)arrancarlo tras cada cambio de `test.env`:

```sh
cat > /opt/zofidesk-test/run.sh <<'EOF'
#!/bin/sh
docker rm -f zofidesk-test 2>/dev/null
docker run -d --name zofidesk-test --network host \
  --env-file /opt/zofidesk-test/test.env \
  -v /opt/zofidesk-test/data:/data \
  ghcr.io/zofitax/zofidesk-server:0.1.0-rc3
EOF
chmod +x /opt/zofidesk-test/run.sh
/opt/zofidesk-test/run.sh
docker logs zofidesk-test
```

En el log tiene que salir:
- la misma `Key:` que en producción;
- `Listening on tcp :31117` (el relay);
- `ZofiDesk API listening on 0.0.0.0:31114`.

Abrir temporalmente en el firewall: **31114/tcp, 31115/tcp, 31116/tcp+udp, 31117/tcp**.

> En la prueba la API va por HTTP en 31114, sin Caddy. Usa solo contraseñas de prueba y borra
> los usuarios al terminar.

Usuarios de prueba:

```sh
docker exec -it zofidesk-test zofidesk-admin user add tecnico1
docker exec -it zofidesk-test zofidesk-admin user add tecnico2
```

## 2. Clientes

En **A y B**, en Ajustes → Red → Servidor ID/Relay:

- Servidor ID: `zigno.zofi.tax:31116`
- Servidor relay: vacío (lo indica el servidor)
- Servidor API: vacío (el cliente usa `http://zigno.zofi.tax:31114`)
- Clave: la de siempre (no cambia)

Comprueba que los dos aparecen "Listo" y anota el ID de B. Mientras dure la prueba, A y B solo
son accesibles a través del servidor de prueba.

## 3. Casos

Para cambiar de fase: edita `test.env`, ejecuta `run.sh` y comprueba en `docker logs` el valor de
`MUST_LOGIN` y de `ALWAYS_USE_RELAY`. Para seguir el log: `docker logs -f zofidesk-test`.

### Fase 1: `MUST_LOGIN=N`, `ALWAYS_USE_RELAY=N` (compatibilidad)

| # | Acción | Resultado esperado |
|---|---|---|
| 1 | A **sin sesión** se conecta a B | Conecta, como hoy |
| 2 | A inicia sesión como `tecnico1` (Ajustes → Cuenta) | Aparece el usuario; sin errores |
| 3 | A con sesión se conecta a B | Conecta |

### Fase 2: `MUST_LOGIN=Y`, `ALWAYS_USE_RELAY=N`

| # | Acción | Resultado esperado |
|---|---|---|
| 4 | A **sin sesión** se conecta a B | Error *"Debe iniciar sesión en ZofiDesk para conectarse a otros equipos"*. Log: `Refused connection from … to <ID B>: not signed in` |
| 5 | A inicia sesión como `tecnico1` y se conecta a B | Conecta. Log: `User tecnico1 connecting from … to <ID B>` |
| 6 | B sigue sin sesión | B sigue "Listo" y acepta la conexión del caso 5 |
| 7 | A cierra sesión y vuelve a conectar | Error del caso 4, de inmediato |
| 8 | A inicia sesión como `tecnico2` y conecta (OK). Luego `docker exec zofidesk-test zofidesk-admin user disable tecnico2`, esperar 30 s y abrir una conexión nueva | La nueva conexión falla con *"Su sesión de ZofiDesk ha caducado. Vuelva a iniciar sesión"* |
| 9 | Durante el caso 8, la sesión remota que ya estaba abierta | **Sigue abierta**: el control actúa al conectar, no corta sesiones en curso |

### Fase 3: `MUST_LOGIN=Y`, `ALWAYS_USE_RELAY=Y` (todo por relay)

| # | Acción | Resultado esperado |
|---|---|---|
| 10 | A con sesión (`tecnico1`) se conecta a B | Conecta por relay. Log: `Relayrequest <uuid> from … got paired`, y ninguna línea `Relay request … refused` |
| 11 | En la sesión del caso 10, transferir un fichero y usar el portapapeles | Funciona (tráfico real por el relay) |
| 12 | A sin sesión se conecta a B | Error del caso 4; nunca llega al relay |

### Fase 4: `MUST_LOGIN=N`, `ALWAYS_USE_RELAY=Y` (compatibilidad por relay)

| # | Acción | Resultado esperado |
|---|---|---|
| 13 | A **sin sesión** se conecta a B | Conecta por relay, como hoy |

## 4. Qué guardar si algo falla

- El log del servidor: `docker logs zofidesk-test > servidor.log`.
- Los logs de ZofiDesk en A y en B. En Windows están en `%AppData%\ZofiDesk\log`. Si ZofiDesk
  está instalado como servicio (lo normal en B), la parte del servicio escribe en
  `C:\Windows\ServiceProfiles\LocalService\AppData\Roaming\ZofiDesk\log`.
- La hora de cada intento y el número de caso.

En el log de A, `request relay attempt` significa que A pidió el relay, y `relay requested from
peer` que lo propuso B. Son los dos caminos que autoriza el servidor.

## 5. Limpieza

```sh
docker rm -f zofidesk-test
sudo rm -rf /opt/zofidesk-test   # incluye la copia de la clave
```

- Cerrar en el firewall los puertos 31114–31117.
- En A y B, vaciar el campo Servidor ID para volver al valor por defecto (`zigno.zofi.tax`), y
  cerrar la sesión de A.

## Qué no cubre esta prueba

- Un intento de usar el relay con un uuid no autorizado. Eso requiere un cliente modificado y
  está cubierto por los tests automáticos (`relay_needs_an_authorized_uuid`).
- HTTPS a través de Caddy: se comprueba en el despliegue real (paso 6 del README).

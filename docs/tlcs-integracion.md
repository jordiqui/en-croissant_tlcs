# Integración de TLCS y `tlc_server16a.exe` mediante WireGuard

Este documento resume una propuesta para integrar un cliente TLCS en En Croissant y conectarlo a un `tlc_server16a.exe` a través de un túnel WireGuard (por ejemplo, levantado con `node-tlcv`), con el objetivo de retransmitir torneos en directo sin depender de WinBoard.

## Objetivos

- Sustituir el flujo WinBoard+adaptador UCI por un cliente TLCS nativo dentro de la app.
- Permitir que la GUI se conecte a un `tlc_server16a.exe` accesible únicamente por VPN/WireGuard.
- Exponer partidas/torneos en tiempo real para retransmisión y análisis desde En Croissant.

## Componentes implicados

- **`tlc_server16a.exe`**: servidor TLCS clásico. Utiliza `servir.ini` para la configuración y puede generar `debug.log` para diagnóstico.
- **WireGuard + `node-tlcv`**: crea el túnel y reexpone el puerto TLCS del servidor remoto. Requiere puerto abierto (por defecto 1965) y claves público/privadas de WireGuard.
- **Cliente TLCS en En Croissant**: nuevo módulo que hable el protocolo TLCS sobre TCP dentro del proceso Tauri (`src-tauri`).

## Flujo de red propuesto

```
GUI (React) ⇄ Comandos Tauri ⇄ Cliente TLCS (Rust) ⇄ Socket TCP (WireGuard) ⇄ tlc_server16a.exe
```

1. El frontend solicita conexión/progreso por comandos Tauri (`invoke`).
2. El backend Tauri abre el socket hacia el servidor TLCS atravesando WireGuard (IP privada del túnel) y gestiona keep-alives y reconexiones.
3. Los mensajes TLCS entrantes (pares de jugadas, estado de reloj, resultados) se normalizan a eventos que el frontend pueda mostrar o reenviar para retransmisión.

## Plan de integración

1. **Configurar el servidor TLCS**
   - Ajustar `servir.ini` con IP privada de WireGuard, puerto TLCS y rutas de libro/log (ver ejemplo en `docs/tlcs-servir-config.md`).
   - Activar el log de depuración (`debug.log`) para inspeccionar formato de mensajes durante la implementación.

2. **Establecer el túnel WireGuard**
   - Crear peer del servidor con `node-tlcv` y exponer el puerto TLCS en la interfaz del túnel (ej.: `10.0.0.2:1965`).
   - Validar conectividad con `nc 10.0.0.2 1965` desde la máquina de desarrollo; revisar `debug.log` si no hay handshake.

### Configuración concreta con `node-tlcv`

Sugerencia de configuración mínima para enlazar el servidor TLCS en la IP privada `10.0.0.2/32` y puerto 1965:

```bash
# En el host que ejecuta node-tlcv
wg set wg-tlcs peer <pubkey-del-dev> allowed-ips 10.0.0.3/32  # peer de la máquina de desarrollo
wg set wg-tlcs peer <pubkey-del-servidor> allowed-ips 10.0.0.2/32

# Anclar la dirección de la interfaz
ip address replace 10.0.0.2/32 dev wg-tlcs

# Reenviar el puerto TLCS hacia el proceso tlc_server16a.exe (si vive fuera del namespace del túnel)
iptables -t nat -A PREROUTING -i wg-tlcs -p tcp --dport 1965 -j DNAT --to-destination 10.0.0.2:1965
iptables -A FORWARD -i wg-tlcs -p tcp --dport 1965 -j ACCEPT
```

Reemplaza `wg-tlcs` con el nombre de la interfaz de WireGuard que cree `node-tlcv`. Si utilizas `ufw`, habilita explícitamente el puerto en la interfaz del túnel:

```bash
ufw allow in on wg-tlcs to 10.0.0.2 port 1965 proto tcp
ufw reload
```

### Pruebas y diagnóstico

1. Desde la máquina de desarrollo, prueba la conectividad:
   ```bash
   nc -vz 10.0.0.2 1965
   ```
   Si el handshake falla, inspecciona `debug.log` en el host que ejecuta `tlc_server16a.exe` para verificar que el servidor está escuchando y que el puerto coincide.
2. Comprueba el estado del túnel:
   ```bash
   wg show wg-tlcs
   ```
   Verifica que los peers tienen `latest handshake` reciente y que el contador de bytes recibidos aumenta tras el `nc`.
3. Si no hay tráfico, revisa las reglas de firewall/NAT y que `net.ipv4.ip_forward=1` esté habilitado.

3. **Implementar el cliente TLCS en `src-tauri`**
   - Crear un servicio Rust (p. ej. `tlcs_client.rs`) que abra sockets TCP y maneje el framing TLCS (mensajes terminados en `\r\n`).
   - Incluir comandos Tauri para `connect`, `subscribe_game`, `send_move`, `keep_alive` y `disconnect`.
   - Reutilizar el canal de eventos de Tauri (`app_handle.emit_all`) para notificar al frontend de nuevas jugadas, estado de conexión o errores.

4. **Adaptar el frontend**
   - Añadir una vista/diálogo de conexión TLCS con parámetros: host WireGuard, puerto, credenciales y flags de reconexión.
   - Presentar la partida en directo (board, relojes, estado) y permitir acciones básicas (aceptar/abandonar, oferta de tablas si el protocolo lo soporta).

5. **Retransmisión y registro**
   - Volcar el stream TLCS a PGN en disco para reproducibilidad y, en paralelo, exponerlo a los módulos existentes de análisis.
   - Registrar tráfico en un `tlcs.log` rotativo (nivel info/debug) para soporte y diagnóstico post-mortem.

## Consideraciones adicionales

- **Compatibilidad**: TLCS y WinBoard/XBoard difieren; evitar traducir a UCI a menos que sea necesario. Si se requiere compatibilidad con motores UCI, encapsular la traducción en el backend para no mezclar lógica en el frontend.
- **Tolerancia a fallos**: implementar reintentos con backoff y detección de latencia alta del túnel; cerrar limpiamente el socket al suspender/cerrar la app.
- **Seguridad**: el túnel WireGuard evita exponer el servidor TLCS públicamente; aun así, validar inputs del usuario y limitar comandos permitidos antes de reenviarlos.
- **Pruebas**: montar un entorno local con `tlc_server16a.exe` en VM Windows y `node-tlcv` en la host Linux para capturar trazas reales y asegurar que el framing y los tiempos de reloj son correctos.

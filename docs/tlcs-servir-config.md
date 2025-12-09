# Configuración y verificación de `servir.ini`

La instancia de `tlc_server16a.exe` se debe publicar únicamente a través de la interfaz privada de WireGuard. Este ejemplo de `servir.ini` fija la IP de escucha en `10.0.0.2`, el puerto TLCS en `1965` y las rutas locales para libro y logs:

```ini
; TLCS server configuration for WireGuard exposure
[network]
bind_address = 10.0.0.2
port = 1965
max_clients = 32

[paths]
book = ./data/books/tlcs-opening-book.abk
log = ./data/logs/tlcs.log

[debug]
enabled = true
file = ./data/logs/debug.log
max_size_mb = 10
rotate = 5
log_handshakes = true
```

## Pasos operativos

1. Copiar `servir.ini` junto a `tlc_server16a.exe` (o apuntar con rutas absolutas si corre como servicio).
2. Crear las carpetas de datos si no existen: `mkdir -p /var/lib/tlcs/books /var/log/tlcs`.
3. Reiniciar el servicio para tomar la nueva configuración (ejemplos):
   ```bash
   systemctl restart tlcs.service
   # o
   ./tlc_server16a.exe
   ```
4. Verificar que el servidor está escuchando en la IP de WireGuard y el puerto 1965:
   ```bash
   ss -tlnp | grep 1965
   ```
5. Abrir una sesión de prueba (por ejemplo `nc 10.0.0.2 1965`) y confirmar que `data/logs/debug.log` recibe entradas de handshake/conexión. El archivo rota al alcanzar ~10 MiB y conserva 5 copias anteriores.

> Nota: el repositorio incluye `data/books` y `data/logs` para facilitar la configuración local; ajuste las rutas si el servicio se despliega en otra ubicación.

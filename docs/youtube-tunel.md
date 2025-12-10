# Túnel a YouTube para retransmitir torneos de En Croissant

Este procedimiento permite tomar el stream de partidas que genera En Croissant (vía TLCS sobre WireGuard) y enviarlo como vídeo en directo a un canal de YouTube utilizando `ffmpeg` u OBS. Incluye también los comandos básicos para compilar la app con MinGW64 en Windows.

## Flujo recomendado

1. **Túnel TLCS**: el servidor `tlc_server16a.exe` queda expuesto en una IP privada de WireGuard (p. ej. `10.0.0.2:1965`) usando `node-tlcv` o una VPN equivalente.
2. **Cliente en En Croissant**: la app se conecta al servidor TLCS a través del túnel y muestra el torneo en vivo.
3. **Restream a YouTube**: se captura la ventana de En Croissant y se envía por RTMP al endpoint de YouTube Live con `ffmpeg` (o bien se usa OBS si prefieres una interfaz gráfica).

> Si necesitas más contexto sobre el túnel TLCS+WireGuard, revisa [docs/tlcs-integracion.md](./tlcs-integracion.md) y [docs/tlcs-servir-config.md](./tlcs-servir-config.md).

## Prerrequisitos

- Clave de emisión de tu canal de YouTube (`live2`), disponible en Studio → Emisión → Configuración del stream.
- `ffmpeg` instalado (en Windows se puede descargar el binario estático y añadirlo al `PATH`).
- Acceso al servidor TLCS a través de WireGuard (par de claves configurado y puerto 1965 accesible en la IP privada del túnel).
- (Opcional) Un dispositivo de captura de audio virtual (VB-Cable o similar) si quieres enviar sonido de sistema o de casters.

## Pasos de red para el túnel

1. Levanta la interfaz de WireGuard y fija la IP privada del servidor TLCS. Ejemplo en el host que ejecuta `node-tlcv`:
   ```bash
   ip address replace 10.0.0.2/32 dev wg-tlcs
   iptables -t nat -A PREROUTING -i wg-tlcs -p tcp --dport 1965 -j DNAT --to-destination 10.0.0.2:1965
   iptables -A FORWARD -i wg-tlcs -p tcp --dport 1965 -j ACCEPT
   ```
2. Comprueba conectividad desde la máquina que ejecutará En Croissant:
   ```bash
   nc -vz 10.0.0.2 1965
   ```
3. En la GUI, indica **Host TLCS** = `10.0.0.2` y **Puerto** = `1965`; activa la reconexión automática si el túnel fluctúa.

## Envío a YouTube con `ffmpeg` (Windows)

Desde una consola `PowerShell` o `cmd` con `ffmpeg` en el `PATH`, sustituye `TU_STREAM_KEY` por tu clave de YouTube:

```bash
ffmpeg ^
  -f gdigrab -framerate 30 -offset_x 0 -offset_y 0 -video_size 1920x1080 -i title="En Croissant" ^
  -f dshow -i audio="virtual-audio-capturer" ^
  -c:v libx264 -preset veryfast -b:v 4500k ^
  -c:a aac -b:a 160k ^
  -f flv "rtmp://a.rtmp.youtube.com/live2/TU_STREAM_KEY"
```

- `gdigrab` captura la ventana de En Croissant; ajusta `title="En Croissant"` o usa `-i desktop` si prefieres la pantalla completa.
- `virtual-audio-capturer` es un ejemplo de dispositivo de audio virtual; cámbialo por el que tengas o elimina la línea si no quieres audio.
- Ajusta `-b:v` y `-framerate` según tu ancho de banda. Para streams de baja latencia usa `-tune zerolatency` y reduce el `preset` si tu CPU lo permite.
- Si prefieres OBS, basta con definir el **Servidor** `rtmp://a.rtmp.youtube.com/live2` y pegar la misma clave de stream.

## Comandos de compilación en MSYS2 MinGW64 (Windows)

1. Abre la shell **MSYS2 MinGW64**.
2. Instala toolchain y dependencias:
   ```bash
   pacman -Sy --needed base-devel git \
     mingw-w64-x86_64-{toolchain,pkg-config,cmake,ninja,python,nodejs,openssl,ffmpeg}
   ```
3. Instala Rust para el target GNU y añade el destino si no aparece por defecto:
   ```bash
   rustup default stable-x86_64-pc-windows-gnu
   rustup target add x86_64-pc-windows-gnu
   ```
4. Instala `pnpm` y prepara el proyecto:
   ```bash
   npm install -g pnpm
   git clone https://github.com/franciscoBSalgueiro/en-croissant.git
   cd en-croissant
   pnpm install
   ```
5. Compila la app (generará el ejecutable en `src-tauri/target/release`):
   ```bash
   pnpm tauri build
   ```

Con estos pasos tendrás el túnel activo hacia TLCS y la retransmisión hacia tu canal de YouTube lista para iniciar desde `ffmpeg` u OBS.

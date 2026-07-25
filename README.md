# Lemonade Server Snap

A snap package for [Lemonade Server](https://lemonade-server.ai/) - a lightweight, high-performance local AI inference server with an OpenAI-compatible API.

## Installation

```bash
sudo snap install lemonade-server
```

## Features

- **OpenAI-compatible API** - Drop-in replacement for OpenAI API endpoints
- **Multiple backends** - Vulkan, ROCm (AMD GPUs), and CPU support
- **Automatic model management** - Downloads and caches models from Hugging Face
- **Background service** - Runs automatically on system startup
- **Hardware acceleration** - Optimized for AMD GPUs with ROCm support

## Supported Hardware

| Backend | Hardware | Architecture |
|---------|----------|--------------|
| Vulkan | Any Vulkan-capable GPU | - |
| ROCm | AMD RX 7000 series (RDNA3) | gfx110X |
| ROCm | AMD RX 9000 series (RDNA4) | gfx120X |
| ROCm | AMD Strix Point APUs | gfx1150 |
| ROCm | AMD Strix Halo APUs | gfx1151 |
| CPU | Any x86_64 processor | - |

## Quick Start

After installation, the server starts automatically and listens on port 13305.

The snap automatically detects your GPU and selects the best backend:
- **AMD GPUs** - Uses ROCm for optimal performance
- **Other GPUs** - Uses Vulkan

### GPU Drivers (Vulkan)

Vulkan support is provided by the `mesa-2404` content snap, which is installed
automatically. If it isn't connected, connect it manually:

```bash
sudo snap connect lemonade-server:gpu-2404 mesa-2404:gpu-2404
sudo snap restart lemonade-server.daemon
```

### Check service status

```bash
sudo snap services lemonade-server
```

### View logs

```bash
sudo snap logs -f lemonade-server.daemon
```

### Test the API

```bash
curl http://localhost:13305/api/v1/models
```

## Usage

### Load a model

```bash
curl -X POST http://localhost:13305/api/v1/load \
  -H "Content-Type: application/json" \
  -d '{"model_name": "Llama-3.2-3B-Instruct-GGUF"}'
```

### Chat completion

```bash
curl -X POST http://localhost:13305/api/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "Llama-3.2-3B-Instruct-GGUF",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

### List available models

```bash
curl http://localhost:13305/api/v1/models
```

## Configuration

### Model cache location

Models are cached in `/var/snap/lemonade-server/common/.cache/huggingface/`.

### Using external models

To use models from your home directory or removable media, the snap has access to:
- `$HOME` (via the `home` plug)
- `/media` and `/mnt` (via the `removable-media` plug)

### API authentication

By default, the server's API endpoints are unauthenticated. To require a
bearer token, set an API key via snap config:

```bash
sudo snap set lemonade-server lemonade-api-key=your-secret-key
```

This secures the regular API endpoints (`/api/*`, `/v0/*`, `/v1/*`). To also
separate access to internal control endpoints (`/internal/*`, e.g. shutdown
and configuration), set a distinct admin key:

```bash
sudo snap set lemonade-server lemonade-admin-api-key=your-admin-secret
```

If only an admin key is set, it also authenticates the regular endpoints. The
daemon restarts automatically to pick up the new key. To clear a key:

```bash
sudo snap unset lemonade-server lemonade-api-key
sudo snap unset lemonade-server lemonade-admin-api-key
```

See the [authentication guide](https://lemonade-server.ai/) for full details
on the auth hierarchy.

### Manual server control

```bash
# Stop the service
sudo snap stop lemonade-server.daemon

# Start the service
sudo snap start lemonade-server.daemon

# Restart the service
sudo snap restart lemonade-server.daemon

# Disable autostart
sudo snap stop --disable lemonade-server.daemon
```

### Command-line usage


```bash
lemonade-server --help
```

## Connecting to Applications

Lemonade Server is compatible with any application that supports the OpenAI API:

- **Open WebUI** - Set API base URL to `http://localhost:13305/api/v1`
- **AnythingLLM** - Configure as OpenAI-compatible endpoint
- **Continue (VS Code)** - Use OpenAI provider with custom base URL
- **LangChain** - Use `ChatOpenAI` with `base_url="http://localhost:13305/api/v1"`

## Troubleshooting

### Service won't start

Check the logs for errors:
```bash
sudo journalctl -u snap.lemonade-server.daemon.service --no-pager -n 50
```

### GPU not detected

Ensure the `gpu-2404` interface is connected:
```bash
sudo snap connect lemonade-server:gpu-2404 mesa-2404:gpu-2404
sudo snap restart lemonade-server.daemon
```

### Permission issues

The snap runs in strict confinement. If you need to access files outside the allowed locations, you may need to copy them to your home directory first.

### Model download failures

Check your network connection and ensure you have enough disk space:
```bash
df -h /var/snap/lemonade-server/common/
```

## Building from Source

```bash
git clone https://github.com/lemonade-sdk/lemonade-server.git
cd lemonade-server
snapcraft
sudo snap install --dangerous lemonade-server_*.snap
```

## Links

- [Lemonade Server Documentation](https://lemonade-server.ai/)
- [Lemonade SDK GitHub](https://github.com/lemonade-sdk/lemonade)
- [Report Issues](https://github.com/lemonade-sdk/lemonade/issues)

## License

Snap packaging is GPL-3 License - See [LICENSE](LICENSE) for details.

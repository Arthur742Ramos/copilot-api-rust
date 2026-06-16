# copilot-api

A Rust port of [copilot-api](https://github.com/ericc-ch/copilot-api): a gateway
that wraps GitHub Copilot and exposes OpenAI- and Anthropic-compatible HTTP APIs.

## Quick start

```sh
# Authenticate against GitHub Copilot
copilot-api auth

# Start the server (defaults to port 4141)
copilot-api start
```

The server stores its config and tokens under
`$HOME/.local/share/copilot-api` by default. Set `COPILOT_API_HOME` to relocate
this directory (useful in containers).

## Docker

```sh
docker build -t copilot-api .
docker run --rm -p 4141:4141 -v copilot-api-data:/data copilot-api
```

State is persisted to the `/data` volume (`COPILOT_API_HOME=/data` in the image).
The container exposes a healthcheck on `http://localhost:4141/`.

## License

MIT

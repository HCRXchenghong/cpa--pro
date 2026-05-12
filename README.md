# cpa-pro

`cpa-pro` packages CLIProxyAPI and kiro-rs together:

- CLIProxyAPI is the public API gateway on port `8317`.
- kiro-rs is the local Kiro OAuth + Anthropic-compatible upstream on port `8990`.
- The installer wires CLIProxyAPI's `claude-api-key` upstream to kiro-rs automatically.

No real credentials are committed. Runtime keys and Kiro OAuth credentials live under `/etc/cpa-pro` and `/var/lib/cpa-pro` on the deployed server.

## One-command Ubuntu install

After this repo is pushed to GitHub, run on a fresh Ubuntu server:

```bash
curl -fsSL https://raw.githubusercontent.com/YOUR_ACCOUNT/cpa-pro/main/scripts/install-ubuntu.sh \
  | sudo CPA_PRO_REPO=https://github.com/YOUR_ACCOUNT/cpa-pro.git bash
```

With a domain pointed at the server:

```bash
curl -fsSL https://raw.githubusercontent.com/YOUR_ACCOUNT/cpa-pro/main/scripts/install-ubuntu.sh \
  | sudo CPA_PRO_REPO=https://github.com/YOUR_ACCOUNT/cpa-pro.git \
      CPA_PRO_DOMAIN=api.example.com \
      bash
```

This prints public URLs such as `http://api.example.com:8317` and `http://api.example.com:8990/admin`. If you put HTTPS/reverse proxy in front of the services, pass the final public URLs explicitly:

```bash
sudo CPA_PRO_API_URL=https://api.example.com \
  CPA_PRO_ADMIN_URL=https://kiro.example.com \
  ./scripts/install-ubuntu.sh
```

Or from a cloned checkout:

```bash
sudo ./scripts/install-ubuntu.sh
```

The installer:

- installs system packages, Node.js, pnpm, Rust, Go, and Kiro CLI;
- builds `kiro-rs` and `CLIProxyAPI` from source;
- generates fresh API keys;
- writes systemd services;
- starts both services.

## Important paths

```text
/opt/cpa-pro/app                         source/build checkout
/etc/cpa-pro/secrets.env                 generated API keys
/etc/cpa-pro/kiro-rs/config.json         kiro-rs config
/etc/cpa-pro/kiro-rs/credentials.json    Kiro credentials
/etc/cpa-pro/cli-proxy-api/config.yaml   CLIProxyAPI config
/var/lib/cpa-pro                         service user home and OAuth storage
```

## After install

Print generated keys:

```bash
sudo cat /etc/cpa-pro/secrets.env
```

Open the Kiro admin panel:

```text
http://SERVER_IP:8990/admin
```

Use `KIRO_ADMIN_KEY` from `secrets.env`.

Click `添加新 Kiro 账号`. On a remote/headless server, open the shown official Kiro login URL in your local browser. If the browser ends at `http://localhost:3128/oauth/callback?...`, copy that full URL back into the `提交回调` box in Kiro Admin. The server will submit it to its local Kiro CLI callback listener and import the credential.

Then use CLIProxyAPI:

```bash
export ANTHROPIC_BASE_URL=http://SERVER_IP:8317
export ANTHROPIC_API_KEY=<CPA_API_KEY from /etc/cpa-pro/secrets.env>
```

If you installed with `CPA_PRO_DOMAIN` or `CPA_PRO_API_URL`, use the printed `CLIProxyAPI` URL as `ANTHROPIC_BASE_URL`.

The internal CLIProxyAPI-to-kiro-rs upstream stays `http://127.0.0.1:8990` by design. The official Kiro OAuth callback also stays `http://localhost:3128/oauth/callback?...`; on a remote server, copy that full callback URL into the Kiro Admin `提交回调` box.

CPA's built-in Gemini and Antigravity OAuth client credentials are not committed. If you need those providers, put your Google OAuth values in `/etc/cpa-pro/secrets.env` and restart `cpa-pro-cli-proxy-api`.

Default model aliases include:

- `claude-sonnet-4-6`
- `claude-sonnet-4-6-thinking`
- `kiro-sonnet`

## Service commands

```bash
sudo systemctl status cpa-pro-kiro-rs
sudo systemctl status cpa-pro-cli-proxy-api
sudo journalctl -u cpa-pro-kiro-rs -f
sudo journalctl -u cpa-pro-cli-proxy-api -f
```

## Security checklist before pushing

Run:

```bash
./scripts/sanitize-secrets.sh
```

It verifies that common runtime secret files and known local keys are not present in the repository.

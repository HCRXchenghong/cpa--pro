# cpa-pro

`cpa-pro` 把 CLIProxyAPI 和 kiro-rs 打包在同一个项目里：

- CLIProxyAPI 对外提供 API 网关，默认端口是 `8317`。
- kiro-rs 负责 Kiro OAuth 登录，并在本机提供 Anthropic 兼容接口，默认端口是 `8990`。
- 部署脚本会自动把 CLIProxyAPI 的 `claude-api-key` 上游接到本机 kiro-rs。

仓库里不会提交真实凭证。运行时生成的密钥和 Kiro OAuth 凭证会放在服务器的 `/etc/cpa-pro` 和 `/var/lib/cpa-pro` 下面。

## Ubuntu 一键部署

在全新的 Ubuntu 服务器上运行：

```bash
curl -fsSL https://raw.githubusercontent.com/HCRXchenghong/cpa--pro/main/scripts/install-ubuntu.sh \
  | sudo CPA_PRO_REPO=https://github.com/HCRXchenghong/cpa--pro.git bash
```

如果域名已经解析到服务器，部署时可以带上域名：

```bash
curl -fsSL https://raw.githubusercontent.com/HCRXchenghong/cpa--pro/main/scripts/install-ubuntu.sh \
  | sudo CPA_PRO_REPO=https://github.com/HCRXchenghong/cpa--pro.git \
      CPA_PRO_DOMAIN=api.example.com \
      bash
```

脚本会输出类似下面的公网访问地址：

```text
CLIProxyAPI: http://api.example.com:8317
Kiro Admin: http://api.example.com:8990/admin
```

如果前面还有 Nginx、Caddy 或其他 HTTPS 反向代理，可以直接传最终公网地址：

```bash
sudo CPA_PRO_API_URL=https://api.example.com \
  CPA_PRO_ADMIN_URL=https://kiro.example.com \
  ./scripts/install-ubuntu.sh
```

如果已经克隆了仓库，也可以在仓库目录里运行：

```bash
sudo ./scripts/install-ubuntu.sh
```

部署脚本会自动完成这些事情：

- 安装系统依赖、Node.js、pnpm、Rust、Go 和 Kiro CLI。
- 从源码构建 `kiro-rs` 和 `CLIProxyAPI`。
- 生成新的 API 密钥。
- 写入运行配置和 systemd 服务。
- 启动 `cpa-pro-kiro-rs` 和 `cpa-pro-cli-proxy-api`。

## 重要路径

```text
/opt/cpa-pro/app                         源码和构建目录
/etc/cpa-pro/secrets.env                 自动生成的 API 密钥
/etc/cpa-pro/kiro-rs/config.json         kiro-rs 配置
/etc/cpa-pro/kiro-rs/credentials.json    Kiro 凭证
/etc/cpa-pro/cli-proxy-api/config.yaml   CLIProxyAPI 配置
/var/lib/cpa-pro                         服务用户目录和 OAuth 状态目录
```

## 部署后怎么用

先查看自动生成的密钥：

```bash
sudo cat /etc/cpa-pro/secrets.env
```

打开 Kiro 管理后台：

```text
http://<服务器IP>:8990/admin
```

登录后台时使用 `secrets.env` 里的 `KIRO_ADMIN_KEY`。

进入后台后点击 `添加新 Kiro 账号`。如果服务器没有桌面环境，就把后台显示的官方 Kiro 登录链接复制到本地浏览器打开。登录完成后，如果浏览器停在 `http://localhost:3128/oauth/callback?...`，把这个完整回调地址复制回 Kiro 管理后台的 `提交回调` 输入框。服务器会把回调提交给本机 Kiro CLI，并导入凭证。

然后客户端使用 CLIProxyAPI：

```bash
export ANTHROPIC_BASE_URL=http://<服务器IP>:8317
export ANTHROPIC_API_KEY=<填入 /etc/cpa-pro/secrets.env 里的 CPA_API_KEY>
```

如果部署时设置了 `CPA_PRO_DOMAIN` 或 `CPA_PRO_API_URL`，就用安装完成时输出的 `CLIProxyAPI` 地址作为 `ANTHROPIC_BASE_URL`。

CLIProxyAPI 到 kiro-rs 的内部上游地址会保持为 `http://127.0.0.1:8990`，这个不需要改成域名。Kiro 官方 OAuth 回调也仍然是 `http://localhost:3128/oauth/callback?...`，远程服务器场景下照样把完整回调地址复制到 Kiro 管理后台提交。

CPA 内置的 Gemini 和 Antigravity OAuth 客户端凭证不会提交到仓库。如果需要使用这些供应商，把自己的 Google OAuth 值写进 `/etc/cpa-pro/secrets.env`，然后重启 `cpa-pro-cli-proxy-api`。

默认模型别名包括：

- `claude-sonnet-4-6`
- `claude-sonnet-4-6-thinking`
- `kiro-sonnet`

## 服务管理命令

```bash
sudo systemctl status cpa-pro-kiro-rs
sudo systemctl status cpa-pro-cli-proxy-api
sudo journalctl -u cpa-pro-kiro-rs -f
sudo journalctl -u cpa-pro-cli-proxy-api -f
```

## 推送前安全检查

每次推送前运行：

```bash
./scripts/sanitize-secrets.sh
```

这个脚本会检查常见运行时配置、OAuth 回调、缓存文件和密钥，避免真实凭证被提交到仓库。

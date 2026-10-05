# 浏览器代理部署说明

PT Manager 的浏览器代理支持带账号密码的 HTTP 和 HTTPS **正向代理**。程序管理的 Chrome / Edge 会经代理访问站点，站点看到的是代理服务器的出口 IP。只影响程序管理的浏览器，不会改变 Windows 其他软件的网络出口。

下面以 Debian 12、Docker Compose、Squid 和 HTTPS 代理为例。所有地址、用户名和文件路径都是示例，请换成自己的值；不要将密码、私钥或真实服务器配置提交到仓库。

## 端口和填写方式

| 程序填写 | 默认端口 | 说明 |
| --- | ---: | --- |
| `https://YOUR_SERVER_IP` 或 `https://proxy.example.com` | 443 | 推荐；客户端到代理的连接和代理认证经过 TLS 加密，证书需匹配填写的 IP 或域名 |
| `http://YOUR_SERVER_IP:3128` | 80；此例明确指定 3128 | 明文 HTTP 代理；访问 HTTPS 站点仍可使用 CONNECT，但代理认证不会被 TLS 包裹 |

`https://YOUR_SERVER_IP` 和 `https://YOUR_SERVER_IP:443` 等价。使用非默认端口必须写出端口。域名和 IP 遵守相同规则。Docker Compose 的 `443:3129` 表示服务器监听 443，并把连接送到容器内部的 3129；Windows 只需填写服务器的 443。

下面的示例只开放 HTTPS 443。如果确实需要额外提供 HTTP 3128，可在 `squid.conf` 增加 `http_port 3128`，并在 Compose 的 `ports` 下增加 `"3128:3128"`。这两个入口共用同一个代理账号，按实际需要开放即可。

Lucky 的普通 Web 反向代理规则不负责此处的 CONNECT 正向代理认证。下面让 Squid 直接提供代理入口。

## 准备

1. 准备一台有公网出口的 Debian 12 服务器，安装 Docker 和 Compose，开放 TCP 443。若通过 HTTP-01 签发证书，还需让 TCP 80 在签发和续期时可用。
2. 为**实际填写的 IP 或域名**取得受信任的 TLS 证书。IP 地址需要包含该 IP 的证书，域名证书不能替代。Let's Encrypt 已支持 IP 证书，Certbot 5.4 及以上可用；IP 证书有效期较短，必须配置自动续期。
3. 选择自己的部署目录，以下以 `/root/proxy` 为例。目录中创建 `Dockerfile`、`docker-compose.yml`、`squid.conf`、`htpasswd` 和 `tls/`。`htpasswd` 与 `tls/` 不要上传到 GitHub。

`Dockerfile`：

```dockerfile
FROM debian:12-slim
RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends squid-openssl ca-certificates && \
    rm -rf /var/lib/apt/lists/*
CMD ["squid", "-N", "-f", "/etc/squid/squid.conf"]
```

`docker-compose.yml`：

```yaml
services:
  proxy:
    build: .
    container_name: browser-proxy
    restart: unless-stopped
    ports:
      - "443:3129"
    volumes:
      - ./squid.conf:/etc/squid/squid.conf:ro
      - ./htpasswd:/etc/squid/htpasswd:ro
      - ./tls:/etc/squid/tls:ro
    security_opt:
      - no-new-privileges:true
    mem_limit: 256m
```

`squid.conf`：

```conf
https_port 3129 tls-cert=/etc/squid/tls/fullchain.pem tls-key=/etc/squid/tls/privkey.pem
visible_hostname browser-proxy
auth_param basic program /usr/lib/squid/basic_ncsa_auth /etc/squid/htpasswd
auth_param basic children 2 startup=1 idle=1
auth_param basic realm Browser-Proxy
acl authenticated proxy_auth REQUIRED
acl Safe_ports port 80 443
acl SSL_ports port 443
acl CONNECT method CONNECT
acl private_dest dst 0.0.0.0/8 10.0.0.0/8 100.64.0.0/10 127.0.0.0/8 169.254.0.0/16 172.16.0.0/12 192.168.0.0/16 ::1/128 fc00::/7 fe80::/10
http_access deny !Safe_ports
http_access deny CONNECT !SSL_ports
http_access deny manager
http_access deny to_localhost
http_access deny to_linklocal
http_access deny private_dest
http_access allow authenticated
http_access deny all
forwarded_for delete
cache deny all
cache_mem 8 MB
access_log none
cache_store_log none
cache_log /dev/null
```

## 账号、证书与启动

在服务器上用 `htpasswd` 创建自己的代理账号，命令会交互式询问密码；不要把密码写入命令行或 Compose 文件：

```sh
cd /root/proxy
apt-get update
apt-get install apache2-utils
htpasswd -c -B htpasswd YOUR_PROXY_USER
docker compose build
```

从已签发证书复制 `fullchain.pem` 和 `privkey.pem` 到 `tls/`。Squid 容器内的 `proxy` 用户必须能读取 `htpasswd` 和这两个证书文件。把下方证书源路径替换成自己的路径：

```sh
install -d -m 0750 tls
install -m 0644 /YOUR_CERT_PATH/fullchain.pem tls/fullchain.pem
install -m 0640 /YOUR_CERT_PATH/privkey.pem tls/privkey.pem
SQUID_GID=$(docker compose run --rm --entrypoint id proxy -g proxy)
chgrp "$SQUID_GID" htpasswd tls tls/fullchain.pem tls/privkey.pem
chmod 0640 htpasswd tls/privkey.pem
chmod 0750 tls
```

启动前检查配置：

```sh
docker compose run --rm proxy squid -k parse
docker compose up -d
docker compose ps
```

把证书续期任务及“续期后复制证书并执行 `docker exec browser-proxy squid -k reconfigure`”的部署钩子设为定时运行。仅续期磁盘文件而不重载 Squid，运行中的代理仍可能使用旧证书。使用短有效期 IP 证书时，应更频繁检查续期状态和端口 80 的可达性。

在 PT Manager 设置中打开“浏览器代理”，填入 `https://YOUR_SERVER_IP` 或 `https://proxy.example.com` 及刚创建的用户名、密码，保存后关闭程序的专用浏览器，再执行保活。若浏览器仍在沿用旧配置，先完全退出该专用浏览器。

## 验证和排查

- 从 Windows 连接代理并检查网站看到的出口 IP；同时确认不填或填错代理密码时返回 `407 Proxy Authentication Required`。
- TLS 握手失败时，检查地址与证书中的 IP/域名是否一致、证书链是否完整、续期是否成功。
- 连接超时时，检查服务器安全组、防火墙和 Docker 的 TCP 443 映射。
- 网站打不开时，先确认代理证书与认证，再查看 Squid 运行状态和目标网站是否允许该服务器出口。

参考：[Squid `https_port` 配置](https://www.squid-cache.org/Doc/config/https_port/)、[Debian `squid-openssl` 包](https://packages.debian.org/bookworm/squid-openssl)、[Let's Encrypt 的 IP 证书及 Certbot 说明](https://letsencrypt.org/2026/03/11/shorter-certs-certbot)。

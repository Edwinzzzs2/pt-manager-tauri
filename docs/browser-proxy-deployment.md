# 浏览器代理部署：先用一份 Compose 跑通 HTTP

PT Manager 支持带账号密码的 HTTP/HTTPS 正向代理。先用下面这一份 `docker-compose.yml` 就能启动 HTTP 代理，不需要制作镜像、申请证书或额外写配置文件。程序管理的 Chrome / Edge 经过代理访问站点，站点看到服务器的出口 IP。

## 1. 创建 Compose 文件

在 Linux 服务器上新建一个**不提交到 Git 的目录**，保存以下 `docker-compose.yml`。把账号和密码改成自己的强密码：

```yaml
services:
  proxy:
    image: ghcr.io/tarampampam/3proxy:2
    container_name: pt-browser-proxy
    restart: unless-stopped
    environment:
      PROXY_LOGIN: "CHANGE_ME_USER"
      PROXY_PASSWORD: "CHANGE_ME_STRONG_PASSWORD"
      PROXY_PORT: "3128"
      LOG_OUTPUT: "/dev/null"
    ports:
      - "3128:3128/tcp"
    security_opt:
      - no-new-privileges:true
    mem_limit: 256m
```

在该目录执行 `docker compose up -d`，再用 `docker compose ps` 查看容器状态。服务器安全组和防火墙需允许 Windows 电脑访问 TCP 3128。账号密码保存在这份服务器本地文件里，建议只让管理员读取，不要把它上传到 GitHub。

这个镜像的 `PROXY_LOGIN`、`PROXY_PASSWORD` 和 `PROXY_PORT` 是其官方提供的配置项。没有正确账号密码的请求会被拒绝。只映射了 HTTP 代理端口，没有向公网开放镜像自带的 SOCKS 端口。

## 2. 在 PT Manager 中填写并测试

打开“设置 → 浏览器代理”，启用开关，填写：

| 配置项 | 填写内容 |
| --- | --- |
| HTTP / HTTPS 代理地址 | `http://YOUR_SERVER_IP:3128` |
| 代理用户名 | 上面 `PROXY_LOGIN` 的值 |
| 代理密码 | 上面 `PROXY_PASSWORD` 的值 |

点击“测试连接”，程序会用当前填写的配置经过代理访问出口检测服务，并显示出口 IP；这一步不必先保存配置。测试成功后保存设置，关闭程序的专用浏览器，再执行保活，让新配置生效。

“测试连接”会单独发出一次代理请求。保活、测试登录和 CookieCloud 同步打开的专用浏览器也会通过代理访问站点；保活任务结束时会检查并关闭程序的专用浏览器。

HTTP 代理可以通过 CONNECT 访问 HTTPS 网站，但**Windows 到代理服务器之间的代理认证是明文传输**。公网使用时至少设置强密码，并尽量在服务器安全组中把 TCP 3128 限制为自己的 Windows 出口 IP。

## 可选：HTTPS 加密代理

如需加密 Windows 到代理服务器的连接及认证，请看[HTTPS 代理部署步骤](browser-proxy-https.md)。HTTPS 使用受信任的 IP 或域名证书；程序可填写 `https://YOUR_SERVER_IP` 或 `https://proxy.example.com`，省略端口时默认使用 443。Docker 的 `443:3129` 表示服务器的 443 转发到容器的 3129，Windows 仍填写 443。

这两个部署方式可以按需选择。HTTP 的一份 Compose 适合先验证服务器出口；HTTPS 需要增加证书签发和自动续期。

参考：[3proxy 镜像的官方配置说明](https://github.com/tarampampam/3proxy-docker)、[Docker Compose 启动说明](https://docs.docker.com/compose/gettingstarted/)。

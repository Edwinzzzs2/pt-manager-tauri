use super::{log_error, IO_TIMEOUT};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;
use tokio_rustls::rustls::{self, pki_types::ServerName, ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

static TLS_CONFIG: OnceLock<Result<Arc<ClientConfig>, String>> = OnceLock::new();

/// 账号认证和 CONNECT 数据都在 TLS 内发送，目标站点原有的 HTTPS 加密仍保持完整。
pub(super) fn connect(upstream: TcpStream, host: &str) -> Result<TcpStream, String> {
    let config = client_config()?;
    // ServerName 同时支持域名和 IP，并按对应名称校验证书，不跳过证书或主机名检查。
    let server_name = ServerName::try_from(host.to_string())
        .map_err(|_| "HTTPS 代理服务器名称无效".to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "无法启动 HTTPS 代理连接".to_string())?;
    upstream
        .set_nonblocking(true)
        .map_err(|err| format!("设置 HTTPS 代理连接模式失败：{err}"))?;
    let transport = {
        let _guard = runtime.enter();
        tokio::net::TcpStream::from_std(upstream)
            .map_err(|err| format!("创建 HTTPS 代理连接失败：{err}"))?
    };
    let mut tls = runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_secs(10),
            TlsConnector::from(config).connect(server_name, transport),
        )
        .await
        .map_err(|_| "HTTPS 代理 TLS 握手超时，请检查服务器地址和端口".to_string())?
        .map_err(|err| {
            format!("HTTPS 代理 TLS 握手失败：{err}。请检查证书是否受系统信任且匹配代理地址")
        })
    })?;

    // 用一对程序内部的回环连接接入原转发流程；浏览器不需要安装组件或运行额外脚本。
    // TLS 读写由同一异步任务管理，避免把一个 TLS 会话复制到两个线程导致数据损坏。
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|err| format!("创建 HTTPS 代理内部通道失败：{err}"))?;
    let local = TcpStream::connect(listener.local_addr().map_err(|err| err.to_string())?)
        .map_err(|err| format!("连接 HTTPS 代理内部通道失败：{err}"))?;
    let (bridge, _) = listener
        .accept()
        .map_err(|err| format!("接收 HTTPS 代理内部通道失败：{err}"))?;
    drop(listener);
    local
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|err| err.to_string())?;
    local
        .set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|err| err.to_string())?;
    bridge
        .set_nonblocking(true)
        .map_err(|err| err.to_string())?;
    let mut bridge = {
        let _guard = runtime.enter();
        tokio::net::TcpStream::from_std(bridge).map_err(|err| err.to_string())?
    };
    thread::Builder::new()
        .name("https-proxy".to_string())
        .spawn(move || {
            runtime.block_on(async {
                if let Err(err) = tokio::io::copy_bidirectional(&mut bridge, &mut tls).await {
                    log_error(format!("HTTPS 代理加密转发中断：{err}"));
                }
            });
        })
        .map_err(|err| format!("启动 HTTPS 代理转发失败：{err}"))?;
    Ok(local)
}

/// 缓存系统 CA，避免页面同时请求多个资源时重复扫描 Windows 证书库。
fn client_config() -> Result<Arc<ClientConfig>, String> {
    TLS_CONFIG
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            for cert in rustls_native_certs::load_native_certs().certs {
                let _ = roots.add(cert);
            }
            if roots.is_empty() {
                return Err("系统中没有可用的 HTTPS 代理信任证书".to_string());
            }
            // 显式选择提供器，避免依赖同时启用 ring 和 aws-lc 时默认选择产生歧义。
            let config = ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::aws_lc_rs::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .map_err(|err| format!("初始化 HTTPS 代理 TLS 失败：{err}"))?
            .with_root_certificates(roots)
            .with_no_client_auth();
            Ok(Arc::new(config))
        })
        .clone()
}

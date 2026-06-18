// SPDX-License-Identifier: AGPL-3.0-or-later

// src/proxy/template.rs
//! nginx.conf template generator.

use crate::config::{ProxyConfig, expand_path};

/// Generate the full `nginx.conf` as a `String`.
///
/// `backend_port` — the port MIRA's Central Server is listening on.
/// `log_dir`      — directory for nginx access / error logs.
pub fn render(cfg: &ProxyConfig, backend_port: u16, log_dir: &std::path::Path) -> String {
    let pid_path    = expand_path(&cfg.pid_path);
    let log_dir_str = log_dir.to_string_lossy();
    let workers     = &cfg.worker_processes;

    let websocket_location = if cfg.websocket_support {
        concat!(
            "        location /api/v1/stream {\n",
            "            proxy_pass          http://mira_backend;\n",
            "            proxy_http_version  1.1;\n",
            "            proxy_set_header    Upgrade    $http_upgrade;\n",
            "            proxy_set_header    Connection \"upgrade\";\n",
            "            proxy_read_timeout  86400;\n",
            "        }\n",
        )
    } else {
        ""
    };

    let proxy_headers = concat!(
        "        proxy_set_header Host              $host;\n",
        "        proxy_set_header X-Real-IP         $remote_addr;\n",
        "        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;\n",
        "        proxy_set_header X-Forwarded-Proto $scheme;\n",
    );

    let server_section = if cfg.tls.enabled {
        let cert = expand_path(&cfg.tls.cert_path);
        let key  = expand_path(&cfg.tls.key_path);
        let port = cfg.tls.listen_port;
        format!(concat!(
            "    server {{\n",
            "        listen 80;\n",
            "        return 301 https://$host$request_uri;\n",
            "    }}\n\n",
            "    server {{\n",
            "        listen {port} ssl http2;\n\n",
            "        ssl_certificate     {cert};\n",
            "        ssl_certificate_key {key};\n",
            "        ssl_protocols       TLSv1.2 TLSv1.3;\n",
            "        ssl_ciphers         ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256;\n",
            "        ssl_session_cache   shared:SSL:10m;\n",
            "        ssl_session_timeout 1d;\n",
            "        add_header Strict-Transport-Security \"max-age=63072000\" always;\n\n",
            "{proxy_headers}\n",
            "        location / {{\n",
            "            proxy_pass http://mira_backend;\n",
            "        }}\n",
            "{ws}",
            "    }}\n",
        ),
            port          = port,
            cert          = cert.display(),
            key           = key.display(),
            proxy_headers = proxy_headers,
            ws            = websocket_location,
        )
    } else {
        format!(concat!(
            "    server {{\n",
            "        listen 80;\n\n",
            "{proxy_headers}\n",
            "        location / {{\n",
            "            proxy_pass http://mira_backend;\n",
            "        }}\n",
            "{ws}",
            "    }}\n",
        ),
            proxy_headers = proxy_headers,
            ws            = websocket_location,
        )
    };

    format!(concat!(
        "# MIRA nginx configuration — auto-generated, do not edit by hand\n",
        "worker_processes {workers};\n",
        "pid {pid};\n\n",
        "events {{\n",
        "    worker_connections 1024;\n",
        "}}\n\n",
        "http {{\n",
        "    access_log {log_dir}/nginx_access.log combined;\n",
        "    error_log  {log_dir}/nginx_error.log warn;\n\n",
        "    upstream mira_backend {{\n",
        "        server 127.0.0.1:{backend_port};\n",
        "        keepalive 32;\n",
        "    }}\n\n",
        "{server_section}",
        "}}\n",
    ),
        workers        = workers,
        pid            = pid_path.display(),
        log_dir        = log_dir_str,
        backend_port   = backend_port,
        server_section = server_section,
    )
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ProxyConfig, TlsConfig};

    fn basic_config() -> ProxyConfig {
        ProxyConfig {
            enabled:           true,
            nginx_binary:      "/usr/sbin/nginx".to_string(),
            config_path:       "/tmp/mira_nginx.conf".to_string(),
            pid_path:          "/tmp/mira_nginx.pid".to_string(),
            worker_processes:  "auto".to_string(),
            websocket_support: true,
            tls: TlsConfig {
                enabled:     false,
                cert_path:   String::new(),
                key_path:    String::new(),
                listen_port: 443,
            },
        }
    }

    #[test]
    fn renders_without_tls() {
        let cfg = basic_config();
        let out = render(&cfg, 8080, std::path::Path::new("/var/log/mira"));
        assert!(out.contains("listen 80;"));
        assert!(out.contains("server 127.0.0.1:8080;"));
        assert!(!out.contains("ssl_certificate"));
    }

    #[test]
    fn renders_with_tls() {
        let mut cfg = basic_config();
        cfg.tls.enabled     = true;
        cfg.tls.cert_path   = "/etc/ssl/cert.pem".to_string();
        cfg.tls.key_path    = "/etc/ssl/key.pem".to_string();
        cfg.tls.listen_port = 443;
        let out = render(&cfg, 8080, std::path::Path::new("/var/log/mira"));
        assert!(out.contains("ssl_certificate"));
        assert!(out.contains("443 ssl http2"));
        assert!(out.contains("return 301 https://"));
    }

    #[test]
    fn renders_websocket_block_when_enabled() {
        let cfg = basic_config();
        let out = render(&cfg, 8080, std::path::Path::new("/tmp"));
        assert!(out.contains("/api/v1/stream"));
        assert!(out.contains("Upgrade"));
    }

    #[test]
    fn renders_without_websocket_when_disabled() {
        let mut cfg = basic_config();
        cfg.websocket_support = false;
        let out = render(&cfg, 8080, std::path::Path::new("/tmp"));
        assert!(!out.contains("/api/v1/stream"));
    }

    #[test]
    fn contains_backend_port() {
        let cfg = basic_config();
        let out = render(&cfg, 9999, std::path::Path::new("/tmp"));
        assert!(out.contains("server 127.0.0.1:9999;"));
    }
}

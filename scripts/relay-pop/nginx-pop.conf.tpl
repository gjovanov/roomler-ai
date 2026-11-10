# nginx for a relay PoP — rendered by provision.sh from pop.env.
#
# TCP/443 is CONTENDED on a PoP: coturn's corp-escape (turns:...:443) and the
# DERP relay's wss:443 both need it. The stream block splits by SNI:
#   coturn-${REGION}. -> passthrough to coturn's own TLS listener (:5349)
#   derp-${REGION}.   -> local TLS termination (:8444) -> plain WS to derp-relay
# UDP/443 stays with coturn via the iptables DNAT (source addr preserved).

user  nginx;
worker_processes  auto;
error_log  /dev/stderr warn;
events { worker_connections 4096; }

stream {
    map $ssl_preread_server_name $relay_backend {
        ${COTURN_HOST}   127.0.0.1:5349;
        ${DERP_HOST}     127.0.0.1:8444;
        default          127.0.0.1:5349;
    }
    server {
        listen 443;
        proxy_pass $relay_backend;
        ssl_preread on;
        proxy_timeout 24h;
    }
}

http {
    # The path, never the query. A daemon older than 0.4.210 dials
    # `/derp?ticket=<its admission ticket>`, and nginx's default `combined`
    # format logs the whole request line. `$uri` is the path alone, and no
    # request header is logged, so a ticket in `Authorization: Bearer` never
    # is either. (nginx's error log still names the full request line when
    # the relay is unreachable; only the header form keeps a ticket out of it.)
    log_format pop '$remote_addr [$time_local] "$request_method $uri $server_protocol" '
                   '$status $body_bytes_sent $request_time "$http_user_agent"';
    access_log /var/log/nginx/access.log pop;

    # DERP TLS termination -> the derp-relay binary (plain WS on 8443).
    server {
        listen 127.0.0.1:8444 ssl;
        server_name ${DERP_HOST};
        ssl_certificate     /certs/fullchain.pem;
        ssl_certificate_key /certs/key.pem;

        location /healthz {
            proxy_pass http://127.0.0.1:8443/healthz;
        }
        # PoP load snapshot for the API's load-aware region routing.
        location /stats {
            proxy_pass http://127.0.0.1:8443/stats;
        }
        location /derp {
            proxy_pass http://127.0.0.1:8443/derp;
            proxy_http_version 1.1;
            proxy_set_header Upgrade $http_upgrade;
            proxy_set_header Connection "upgrade";
            # An idle DERP socket is legitimately silent between keepalives.
            proxy_read_timeout 86400s;
            proxy_send_timeout 86400s;
        }
    }
}

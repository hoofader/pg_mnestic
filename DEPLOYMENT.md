# Deployment

## TLS is mandatory

The server (`mnestic-server`, `--features serve`) speaks plain HTTP. Auth is a bearer token
in the `Authorization` header, so any request that crosses a network without TLS leaks a
credential that grants full access to a tenant's memories. The server does **not** terminate
TLS itself by design; terminate it at a reverse proxy or load balancer in front of the app.

To stop an accidental plaintext exposure, the binary refuses to bind a non-loopback address
unless you assert that TLS is handled upstream:

- `MNESTIC_BIND` defaults to `127.0.0.1:8080`. A loopback bind always starts. It must be an
  `ip:port`; hostnames (including `localhost`) are rejected, since loopback cannot be checked
  before name resolution and resolution can be spoofed.
- A non-loopback bind (`0.0.0.0:8080`, a LAN/interface IP) fails to start unless you set
  `MNESTIC_TRUST_PROXY=1`, which is your statement that a proxy terminates TLS before traffic
  reaches this socket.

This is a guard, not encryption: setting `MNESTIC_TRUST_PROXY=1` without an actual TLS proxy
in front still exposes cleartext. The flag exists so exposing the port is a deliberate act.

## Recommended topologies

**Proxy on the same host (simplest).** Bind the app to loopback and point the proxy at it.
No flag needed.

```
client --TLS--> proxy (:443 on the host) --HTTP--> 127.0.0.1:8080 (mnestic-server)
```

**App on a private network behind a load balancer.** The LB or ingress terminates TLS; the
app listens on all interfaces inside a network nothing untrusted can reach. Set
`MNESTIC_TRUST_PROXY=1` and keep the app's port closed to the public internet at the firewall.

```
client --TLS--> LB (:443) --HTTP (private net)--> mnestic-server 0.0.0.0:8080
```

## Example: Caddy (automatic certificates)

```caddy
memory.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

## Example: nginx

```nginx
server {
    listen 443 ssl;
    server_name memory.example.com;

    ssl_certificate     /etc/ssl/memory.example.com/fullchain.pem;
    ssl_certificate_key /etc/ssl/memory.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}
```

## Health checks

`GET /health` returns `200 ok` with no auth and no database call. Use it for the proxy's
upstream check and for liveness/readiness probes. It does not assert database connectivity;
a deeper readiness check is a later roadmap item.

## Notes

- In-process TLS (rustls in the app) is intentionally out of scope. Terminating at the edge
  keeps certificate rotation, HSTS, and ALPN with the proxy where ops already manage them.
- Do not expose the app port to the public internet even with `MNESTIC_TRUST_PROXY=1`; the
  proxy is the only intended ingress.

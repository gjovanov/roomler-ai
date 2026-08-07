# GeoIP database (operator-supplied)

The platform user analytics resolves a client's country at WebSocket
connect time and then **drops the address** — no IP is ever stored. That
lookup needs a MaxMind-format database, which is deliberately NOT in this
repository: GeoLite2 is licensed data, and vendoring it would put a
third-party dataset (and its update obligations) into our git history.

Instead the build host drops the `.mmdb` here and the image bakes it in:

```bash
# once per DB refresh, on the build host
tar -xzf ~/GeoLite2-Country_YYYYMMDD.tar.gz -C /tmp
cp /tmp/GeoLite2-Country_*/GeoLite2-Country.mmdb \
   ~/roomler-build/files/geoip/GeoLite2-Country.mmdb
```

`scripts/../stats-image-build.sh` on the build host does this automatically
before `docker build`.

The image then carries it at `/usr/share/roomler/geoip/`, which
`ROOMLER__STATS__GEOIP_MMDB` points at.

**Nothing breaks when it is absent.** A build without the file (CI, a
fresh clone) produces an image whose analytics report `country: unknown`
and whose payload carries `geoip: false`, so the dashboard says "no GeoIP
database" rather than implying every user is in one place. That is the
designed degradation, not an error.

Country granularity is what the analytics uses. The City database also
exists (much larger); adopting it would only make sense alongside a
deliberate decision to record finer-grained location, which is a privacy
choice, not a data-availability one.

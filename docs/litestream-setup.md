# Litestream Disaster Recovery Setup

TinyLord stores each logical database as its own SQLite file, encrypted with
SQLCipher by default. This document describes how to replicate those files
continuously to an S3-compatible object store with Litestream, so a lost or
corrupted server can be restored to a recent point in time.

Paths, bucket names, and the service account below are examples. Substitute
the values your deployment actually uses.

## Requirements

- **A SQLCipher-aware Litestream build.** An unmodified Litestream binary
  cannot read an encrypted TinyLord database and will fail with
  `file is not a database`. You need a build that accepts the database key,
  conventionally through the `LITESTREAM_SQLCIPHER_KEY` environment variable.
  Build it for your target operating system, and prefer the same SQLCipher
  release family and compile options that TinyLord itself uses.
- **An S3-compatible bucket** and an application key with list, read, and
  write access to the backup prefix.
- **The TinyLord encryption key**, stored in a file readable only by the
  service account.

After changing either SQLCipher build — TinyLord's or Litestream's — run the
restore drill below before relying on backups again.

> Never put the TinyLord encryption key or object-store credentials in version
> control, in `litestream.yml`, in shell history, or in systemd unit text.
> Keep them in a mode `0600` file owned by the service account.

## Layout

A deployment needs these files. The exact locations don't matter as long as
the secrets are not world-readable.

| Path | Purpose |
| --- | --- |
| `/var/lib/tinylord/data/` | Encrypted TinyLord databases |
| `/usr/local/bin/litestream-sqlcipher` | SQLCipher-aware Litestream binary |
| `/etc/tinylord/litestream.yml` | Replication configuration, no secrets |
| `/etc/tinylord/secrets/litestream.env` | Object-store credentials, mode `0600` |
| `/etc/tinylord/secrets/tinylord.key` | Raw SQLCipher key, mode `0600` |
| `/etc/systemd/system/litestream-tinylord.service` | systemd unit |

## Credentials

Create a private environment file owned by the service account:

```sh
install -d -m 700 /etc/tinylord/secrets
umask 077
cat > /etc/tinylord/secrets/litestream.env <<'EOF'
LITESTREAM_ACCESS_KEY_ID=replace-me
LITESTREAM_SECRET_ACCESS_KEY=replace-me
EOF
chmod 600 /etc/tinylord/secrets/litestream.env
```

Keep the object-store endpoint in the replica URL rather than the credential
file, so the credential file holds nothing but the key pair.

## Replication Configuration

Give every TinyLord database its own replica prefix. **Include `_system.db`** —
it holds TinyLord's users, permissions, and server settings, and a restore
without it leaves you with data you cannot authenticate against.

```yaml
access-key-id: ${LITESTREAM_ACCESS_KEY_ID}
secret-access-key: ${LITESTREAM_SECRET_ACCESS_KEY}
region: us-east-1
sync-interval: 1s
validation-interval: 24h

snapshot:
  interval: 24h
  retention: 720h

retention:
  enabled: true

logging:
  level: info

dbs:
  - path: /var/lib/tinylord/data/_system.db
    replica:
      url: s3://my-bucket/tinylord/_system
  - path: /var/lib/tinylord/data/app.db
    replica:
      url: s3://my-bucket/tinylord/app
```

Repeat the `dbs` entry for every additional `*.db` file. Use a unique prefix
per database *and* per environment — sharing a prefix between two databases or
between staging and production mixes their replication history and makes both
unrestorable.

Non-AWS S3-compatible providers generally need the endpoint and a few flags in
the replica URL, for example:

```
s3://my-bucket/tinylord/app?endpoint=https://s3.example-provider.com&sign-payload=true&force-path-style=true
```

Set `region` to whatever the provider specifies for the bucket.

## systemd Service

`LITESTREAM_SQLCIPHER_KEY` must use SQLCipher's **raw key** syntax —
`x'<64-hex-character-key>'`, quotes included. Passing the bare hex string makes
SQLCipher treat it as a passphrase to derive a key from, and Litestream will
report `file is not a database`. This is the single most common setup mistake.

Ordering matters: start Litestream after TinyLord, and tie their lifecycles
together so a TinyLord restart doesn't leave replication running against files
that are no longer being written.

```ini
[Unit]
Description=Litestream backup for TinyLord
Wants=network-online.target
After=network-online.target tinylord.service
Requires=tinylord.service
PartOf=tinylord.service

[Service]
Type=simple
User=tinylord
WorkingDirectory=/var/lib/tinylord
EnvironmentFile=/etc/tinylord/secrets/litestream.env
ExecStart=/bin/sh -c 'export LITESTREAM_SQLCIPHER_KEY="x'"'"'$(cat /etc/tinylord/secrets/tinylord.key)'"'"'"; exec /usr/local/bin/litestream-sqlcipher replicate -config /etc/tinylord/litestream.yml'
Restart=on-failure
RestartSec=5
UMask=0077
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

The `ExecStart` quoting is load-bearing: it wraps the key file's contents in
`x'...'` without ever writing the key into the unit file. Install and start it:

```sh
systemctl daemon-reload
systemctl enable --now litestream-tinylord
systemctl status litestream-tinylord --no-pager
```

To run TinyLord as a user service instead, drop the `User=` line, use
`systemctl --user`, and set `WantedBy=default.target`.

## Verify

Normal operation emits `replica sync` messages for every configured database:

```sh
systemctl is-active litestream-tinylord
journalctl -u litestream-tinylord -n 100 --no-pager
```

Check this after every TinyLord deployment or service restart. A database file
added after Litestream started is not replicated until the config lists it and
the service is reloaded.

## Restore Drill

Run a drill after initial setup, after any SQLCipher upgrade, and after changes
to the backup configuration. Restore to a temporary location only — never
restore over a live database.

```sh
set -a
. /etc/tinylord/secrets/litestream.env
set +a
export LITESTREAM_SQLCIPHER_KEY="x'$(cat /etc/tinylord/secrets/tinylord.key)'"

tmp_dir="$(mktemp -d /tmp/tinylord-restore.XXXXXX)"
/usr/local/bin/litestream-sqlcipher restore \
  -o "$tmp_dir/app-restored.db" \
  's3://my-bucket/tinylord/app'
```

Open the restored file with a SQLCipher-aware tool using the same key to
confirm it decrypts and queries. Remove the temporary directory immediately
afterward — it contains a full copy of your data.

## Operational Notes

- Backups contain **encrypted** pages. The object-store credentials are not a
  substitute for the encryption key; protect and back up that key separately,
  somewhere other than the bucket it protects.
- Keep the key stable. Losing it makes the live databases *and* every backup
  permanently unrecoverable.
- A successful upload is not proof that a backup is usable. Only a completed
  restore drill is.
- Replicate `_system.db` alongside your application databases, and restore them
  as a set — a `_system.db` from a different point in time may not match the
  collections and grants in the databases it authorizes.

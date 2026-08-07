# Litestream Disaster Recovery Setup

This document describes the SQLCipher-aware Litestream backup service used by
the TinyLord instance on the server. It continuously replicates every TinyLord
database to an S3-compatible object store and supports point-in-time restore.

## Requirements

- TinyLord encryption is enabled and its key is stored in a protected key file.
- Use the SQLCipher-capable Litestream fork, not an unmodified Litestream
  binary. The fork accepts the database key through `LITESTREAM_SQLCIPHER_KEY`.
- Build Litestream for the target operating system. Prefer the same SQLCipher
  release family and compile options used by TinyLord. After changing either
  SQLCipher build, run the restore drill below before relying on backups.
- An S3-compatible bucket and an application key with list, read, and write
  access to the backup prefix.

Never put the TinyLord encryption key or object-store credentials in this
repository, `litestream.yml`, shell history, or systemd unit text.

## Server Layout

The deployed instance uses these private files:

| Path | Purpose |
| --- | --- |
| `/home/tudisco/DelegateServer/data/` | Encrypted TinyLord databases |
| `/home/tudisco/DelegateServer/litestream-tinylord` | SQLCipher-aware Litestream binary |
| `/home/tudisco/DelegateServer/litestream.yml` | Replication configuration without secrets |
| `/home/tudisco/DelegateServer/secrets/litestream-b2.env` | Object-store credentials, mode `0600` |
| `/home/tudisco/DelegateServer/secrets/tinylord.key` | TinyLord raw SQLCipher key, mode `0600` |
| `/home/tudisco/.config/systemd/user/litestream-tinylord.service` | User systemd service |

The service is enabled as a user service and is tied to
`tinylord-delegate.service`. It automatically restarts after a failure.

## Credentials

Create a private environment file owned by the service account:

```sh
install -d -m 700 /home/tudisco/DelegateServer/secrets
umask 077
cat > /home/tudisco/DelegateServer/secrets/litestream-b2.env <<'EOF'
LITESTREAM_ACCESS_KEY_ID=replace-me
LITESTREAM_SECRET_ACCESS_KEY=replace-me
EOF
chmod 600 /home/tudisco/DelegateServer/secrets/litestream-b2.env
```

For Backblaze B2, use the S3 endpoint and region supplied for the bucket. Keep
the endpoint in the replica URL, not the credential file.

## Replication Configuration

Each TinyLord database must have its own replica prefix. Include `_system.db`:
it holds TinyLord users, permissions, and server settings.

```yaml
access-key-id: ${LITESTREAM_ACCESS_KEY_ID}
secret-access-key: ${LITESTREAM_SECRET_ACCESS_KEY}
region: us-east-005
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
  - path: /home/tudisco/DelegateServer/data/_system.db
    replica:
      url: s3://TinyLord/tinylord/_system?endpoint=https://s3.us-east-005.backblazeb2.com&sign-payload=true&force-path-style=true
  - path: /home/tudisco/DelegateServer/data/delegate.db
    replica:
      url: s3://TinyLord/tinylord/delegate?endpoint=https://s3.us-east-005.backblazeb2.com&sign-payload=true&force-path-style=true
```

Repeat the `dbs` entry for every additional `*.db` file. Use a unique prefix
per database and per TinyLord environment to avoid mixing replication history.

## systemd Service

`LITESTREAM_SQLCIPHER_KEY` must use TinyLord's raw SQLCipher key syntax:
`x'<64-hex-character-key>'`. Passing the hex key as an ordinary passphrase will
make Litestream report `file is not a database`.

```ini
[Unit]
Description=Litestream backup for TinyLord
Wants=network-online.target
After=network-online.target tinylord-delegate.service
Requires=tinylord-delegate.service
PartOf=tinylord-delegate.service

[Service]
Type=simple
WorkingDirectory=/home/tudisco/DelegateServer
EnvironmentFile=/home/tudisco/DelegateServer/secrets/litestream-b2.env
ExecStart=/bin/sh -c 'export LITESTREAM_SQLCIPHER_KEY="x'"'"'$(cat /home/tudisco/DelegateServer/secrets/tinylord.key)'"'"'"; exec /home/tudisco/DelegateServer/litestream-tinylord replicate -config /home/tudisco/DelegateServer/litestream.yml'
Restart=on-failure
RestartSec=5
UMask=0077
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=default.target
```

Install and start it:

```sh
systemctl --user daemon-reload
systemctl --user enable --now litestream-tinylord
systemctl --user status litestream-tinylord --no-pager
```

## Verify and Restore

Normal operation should emit `replica sync` messages for every configured
database:

```sh
systemctl --user is-active litestream-tinylord
journalctl --user -u litestream-tinylord -n 100 --no-pager
```

Perform a restore drill after initial setup, SQLCipher upgrades, or changes to
the backup configuration. Restore to a temporary location only; never restore
over a live database.

```sh
set -a
. /home/tudisco/DelegateServer/secrets/litestream-b2.env
set +a
export LITESTREAM_SQLCIPHER_KEY="x'$(cat /home/tudisco/DelegateServer/secrets/tinylord.key)'"

tmp_dir="$(mktemp -d /tmp/tinylord-restore.XXXXXX)"
/home/tudisco/DelegateServer/litestream-tinylord restore \
  -o "$tmp_dir/delegate-restored.db" \
  's3://TinyLord/tinylord/delegate?endpoint=https://s3.us-east-005.backblazeb2.com&sign-payload=true&force-path-style=true'
```

Open the restored file with a SQLCipher-aware tool using the same key to verify
it decrypts. Remove the temporary restore directory immediately afterward.

## Operational Notes

- Backups contain encrypted database pages. The object-store credentials do not
  replace the TinyLord encryption key; protect and back up that key separately.
- Keep the key stable. Losing it makes both live databases and backups
  unrecoverable.
- Test restores periodically. A successful upload alone is not proof that a
  backup can be used during an incident.
- Check replication after every TinyLord deployment or service restart.

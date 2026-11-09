#!/bin/sh
# Hot backup of the production SQLite database.
#
# The DB lives in the `cryptograph_data` docker volume, i.e. on the host at
# /var/lib/docker/volumes/cryptograph_data/_data/crypto.db. `sqlite3 .backup`
# takes a consistent snapshot even while the bot is writing (WAL mode), which a
# plain `cp` of the .db file would not.
#
# Usage: scripts/backup_db.sh [dest_dir]
# Cron:  15 3 * * * /home/crypto/scripts/backup_db.sh >/dev/null 2>&1
set -eu

VOLUME_DIR="${CRYPTOGRAPH_VOLUME_DIR:-}"
if [ -z "$VOLUME_DIR" ]; then
    # Resolve the real mount of /app/data from the running container. The named
    # volume is project-prefixed (`crypto_cryptograph_data`), so hardcoding a
    # name silently backs up an empty legacy volume.
    VOLUME_DIR=$(docker inspect cryptograph \
        -f '{{range .Mounts}}{{if eq .Destination "/app/data"}}{{.Source}}{{end}}{{end}}' 2>/dev/null || true)
fi
[ -n "$VOLUME_DIR" ] || VOLUME_DIR=/var/lib/docker/volumes/crypto_cryptograph_data/_data
DB="${VOLUME_DIR}/crypto.db"
DEST="${1:-/home/crypto/backups}"
KEEP="${CRYPTOGRAPH_BACKUP_KEEP:-14}"

[ -f "$DB" ] || { echo "backup_db: no database at $DB" >&2; exit 1; }
mkdir -p "$DEST"

STAMP=$(date -u +%Y%m%dT%H%M%SZ)
OUT="${DEST}/crypto-${STAMP}.db"
TMP="${OUT}.tmp"

sqlite3 "$DB" ".backup '$TMP'"
mv "$TMP" "$OUT"

# Integrity check on the copy, not the live file.
if ! sqlite3 "$OUT" "PRAGMA quick_check;" | grep -qx ok; then
    echo "backup_db: integrity check failed for $OUT" >&2
    exit 1
fi

# Rotate: keep the newest $KEEP snapshots.
ls -1t "$DEST"/crypto-*.db 2>/dev/null | tail -n +"$((KEEP + 1))" | while read -r old; do
    rm -f "$old"
done

echo "backup_db: $(basename "$OUT") ($(du -h "$OUT" | cut -f1))"

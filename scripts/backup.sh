#!/usr/bin/env sh
set -eu
stamp=$(date -u +%Y%m%dT%H%M%SZ)
destination="backups/$stamp"
mkdir -p "$destination"
docker compose exec -T postgres pg_dump -U "${POSTGRES_USER:-vault}" -d "${POSTGRES_DB:-clipboard_vault}" -Fc > "$destination/database.dump"
docker run --rm -v clipboard-vault_vault-files:/source:ro -v "$(pwd)/$destination:/backup" alpine \
  tar -czf /backup/files.tar.gz -C /source .
sha256sum "$destination/database.dump" "$destination/files.tar.gz" > "$destination/SHA256SUMS"
echo "Backup saved to $destination"


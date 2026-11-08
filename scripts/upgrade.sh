#!/usr/bin/env sh
# In-place upgrades between Clipboard Vault 3.x releases (Compose installs).
#
# Moving from 2.x (PostgreSQL + auth service) to 3.0 is NOT an in-place upgrade:
# it needs the one-time import described in DEPLOYMENT.md. This script refuses to
# run against a 2.x stack.
set -eu

say() { printf '%s\n' "[clipboard-vault] $*"; }
fail() { printf '%s\n' "[clipboard-vault] ERROR: $*" >&2; exit 1; }
compose() { docker compose -f docker-compose.yml -f docker-compose.prod.yml "$@"; }

if [ "${CLIPBOARD_VAULT_UPGRADE_REEXEC:-0}" != "1" ]; then
  repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || fail "run this command from the Clipboard Vault repository"
  temporary_script=$(mktemp "${TMPDIR:-/tmp}/clipboard-vault-upgrade.XXXXXX")
  cp "$0" "$temporary_script"
  chmod 700 "$temporary_script"
  status=0
  CLIPBOARD_VAULT_UPGRADE_REEXEC=1 CLIPBOARD_VAULT_REPO_ROOT="$repo_root" "$temporary_script" "$@" || status=$?
  rm -f "$temporary_script"
  exit "$status"
fi

REPO_ROOT=${CLIPBOARD_VAULT_REPO_ROOT:?missing repository root}
cd "$REPO_ROOT"
STATE_DIRECTORY="$REPO_ROOT/.deployment"
STATE_FILE="$STATE_DIRECTORY/last-success.env"
PENDING_STATE_FILE="$STATE_DIRECTORY/pending.env"
VAULT_IMAGE="clipboard-vault:local"

usage() {
  cat <<'EOF'
Usage:
  ./scripts/upgrade.sh --check <git-ref>
  ./scripts/upgrade.sh <git-ref>
  ./scripts/upgrade.sh --refresh-base <git-ref>
  ./scripts/upgrade.sh --rollback
EOF
}

MODE=upgrade
REF=
case "${1:-}" in
  --check)
    [ "$#" -eq 2 ] || { usage >&2; exit 2; }
    MODE=check
    REF=$2
    ;;
  --refresh-base)
    [ "$#" -eq 2 ] || { usage >&2; exit 2; }
    MODE=refresh
    REF=$2
    ;;
  --rollback)
    [ "$#" -eq 1 ] || { usage >&2; exit 2; }
    MODE=rollback
    ;;
  -h|--help)
    usage
    exit 0
    ;;
  "")
    usage >&2
    exit 2
    ;;
  *)
    [ "$#" -eq 1 ] || { usage >&2; exit 2; }
    REF=$1
    ;;
esac

version_at_least() {
  actual=$1
  minimum=$2
  first=$(printf '%s\n%s\n' "$minimum" "$actual" | sort -V | head -n 1)
  [ "$first" = "$minimum" ]
}

check_pair() {
  provider=$1
  client_id=$2
  client_secret=$3
  client_id_name=$4
  client_secret_name=$5
  if [ -n "$client_id" ] && [ -z "$client_secret" ]; then
    fail "$provider OAuth configuration is incomplete: $client_secret_name must be set when $client_id_name is set"
  fi
  if [ -z "$client_id" ] && [ -n "$client_secret" ]; then
    fail "$provider OAuth configuration is incomplete: $client_id_name must be set when $client_secret_name is set"
  fi
}

load_environment() {
  [ -f .env ] || fail "create .env from .env.example first"
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
  check_pair Google "${GOOGLE_CLIENT_ID:-}" "${GOOGLE_CLIENT_SECRET:-}" GOOGLE_CLIENT_ID GOOGLE_CLIENT_SECRET
  check_pair GitHub "${GITHUB_CLIENT_ID:-}" "${GITHUB_CLIENT_SECRET:-}" GITHUB_CLIENT_ID GITHUB_CLIENT_SECRET
  check_pair Discord "${DISCORD_CLIENT_ID:-}" "${DISCORD_CLIENT_SECRET:-}" DISCORD_CLIENT_ID DISCORD_CLIENT_SECRET
  [ -n "${PUBLIC_BASE_URL:-}" ] || fail "PUBLIC_BASE_URL is missing from .env"
  [ -n "${VAULT_SECRET:-}" ] || fail "VAULT_SECRET is missing from .env (3.x replaced the 2.x secrets)"
  [ -n "${SETUP_TOKEN:-}" ] || fail "SETUP_TOKEN is missing from .env (it replaces BOOTSTRAP_TOKEN)"
}

check_versions() {
  command -v docker >/dev/null 2>&1 || fail "Docker is not installed"
  docker info >/dev/null 2>&1 || fail "the Docker daemon is unavailable"
  docker_version=$(docker version --format '{{.Server.Version}}')
  compose_version=$(docker compose version --short)
  version_at_least "$docker_version" 25.0.0 || fail "Docker Engine 25.0.0+ is required (found $docker_version)"
  version_at_least "$compose_version" 2.20.2 || fail "Docker Compose 2.20.2+ is required (found $compose_version)"
}

check_clean_checkout() {
  git diff --quiet || fail "tracked files have unstaged changes"
  git diff --cached --quiet || fail "tracked files have staged changes"
  untracked=$(git status --porcelain --untracked-files=normal | grep -Ev '^\?\? (\.deployment/|backups/)' || true)
  [ -z "$untracked" ] || fail "the checkout contains untracked files; move or commit them before upgrading"
}

check_disk_reserve() {
  reserve=${UPLOAD_DISK_RESERVE_BYTES:-21474836480}
  case "$reserve" in *[!0-9]*|"") fail "UPLOAD_DISK_RESERVE_BYTES must be a non-negative integer" ;; esac
  docker_root=$(docker info --format '{{.DockerRootDir}}')
  available_kib=$(df -Pk "$docker_root" | awk 'NR == 2 { print $4 }')
  case "$available_kib" in *[!0-9]*|"") fail "could not determine free space for $docker_root" ;; esac
  available_bytes=$((available_kib * 1024))
  [ "$available_bytes" -gt "$reserve" ] || fail "Docker storage has $available_bytes bytes free; more than the $reserve-byte reserve is required"
}

check_not_2x() {
  # A running 2.x stack has separate PostgreSQL and auth containers.
  if [ -n "$(docker ps -q --filter 'label=com.docker.compose.project=clipboard-vault' --filter 'label=com.docker.compose.service=postgres')" ] ||
     [ -n "$(docker ps -q --filter 'label=com.docker.compose.project=clipboard-vault' --filter 'label=com.docker.compose.service=auth')" ]; then
    fail "this host runs Clipboard Vault 2.x; follow 'Moving from 2.x to 3.0' in DEPLOYMENT.md instead"
  fi
}

check_service() {
  vault_container=$(compose ps --status running -q vault)
  [ -n "$vault_container" ] || fail "the vault service is not running"
}

resolve_ref() {
  requested_ref=$1
  if [ "$MODE" != "check" ]; then
    say "Fetching repository refs" >&2
    git fetch --all --tags --prune
  fi
  git rev-parse --verify "${requested_ref}^{commit}" 2>/dev/null ||
    fail "git ref '$requested_ref' is unavailable; fetch it before running --check"
}

check_target_compose() {
  target_commit_value=$1
  temporary_directory=$(mktemp -d "${TMPDIR:-/tmp}/clipboard-vault-check.XXXXXX")
  case "$temporary_directory" in
    "${TMPDIR:-/tmp}"/clipboard-vault-check.*) ;;
    *) fail "refusing unsafe temporary directory $temporary_directory" ;;
  esac
  git archive "$target_commit_value" | tar -x -C "$temporary_directory"
  target_config_status=0
  docker compose --env-file "$REPO_ROOT/.env" \
    -f "$temporary_directory/docker-compose.yml" \
    -f "$temporary_directory/docker-compose.prod.yml" config --quiet || target_config_status=$?
  rm -rf -- "$temporary_directory"
  [ "$target_config_status" -eq 0 ] || fail "the target Compose configuration is invalid"
}

preflight() {
  check_clean_checkout
  load_environment
  check_versions
  check_not_2x
  compose config --quiet
  check_disk_reserve
  check_service
}

wait_for_readiness() {
  attempt=1
  while [ "$attempt" -le 60 ]; do
    # The runtime image has no shell or curl: the binary checks itself.
    if compose exec -T vault /usr/local/bin/clipboard-vault healthcheck >/dev/null 2>&1 \
      && curl -fsS "${PUBLIC_BASE_URL%/}/health/ready" >/dev/null 2>&1; then
      return 0
    fi
    sleep 2
    attempt=$((attempt + 1))
  done
  return 1
}

write_state() {
  destination=$1
  previous_commit_value=$2
  deployed_commit_value=$3
  vault_tag_value=$4
  snapshot_value=$5
  umask 077
  {
    printf "previous_commit='%s'\n" "$previous_commit_value"
    printf "deployed_commit='%s'\n" "$deployed_commit_value"
    printf "vault_rollback_tag='%s'\n" "$vault_tag_value"
    printf "database_snapshot='%s'\n" "$snapshot_value"
  } > "$destination"
}

restore_image() {
  previous_commit_value=$1
  vault_tag_value=$2
  say "Restoring the previous application image"
  docker image inspect "$vault_tag_value" >/dev/null 2>&1 || fail "rollback image $vault_tag_value is unavailable"
  docker image tag "$vault_tag_value" "$VAULT_IMAGE"
  compose up -d --no-build --force-recreate --wait --wait-timeout 120 vault
  wait_for_readiness || fail "the previous image was restored but readiness verification failed"
  git checkout --detach "$previous_commit_value"
  say "Rollback is healthy at commit $previous_commit_value"
}

if [ "$MODE" = "rollback" ]; then
  preflight
  [ -f "$STATE_FILE" ] || fail "no successful deployment state is available for rollback"
  # shellcheck disable=SC1090
  . "$STATE_FILE"
  [ -n "${previous_commit:-}" ] || fail "rollback state has no previous commit"
  [ -n "${vault_rollback_tag:-}" ] || fail "rollback state has no vault image"
  restore_image "$previous_commit" "$vault_rollback_tag"
  say "Database rows were not rewound. The pre-upgrade snapshot is ${database_snapshot:-unknown} inside the data volume"
  exit 0
fi

preflight
target_commit=$(resolve_ref "$REF")
check_target_compose "$target_commit"
say "Preflight passed for $REF ($target_commit)"
if [ "$MODE" = "check" ]; then
  say "No deployment files, containers, images, or data were changed"
  exit 0
fi

mkdir -p "$STATE_DIRECTORY"
chmod 700 "$STATE_DIRECTORY"
previous_commit=$(git rev-parse HEAD)
timestamp=$(date -u +%Y%m%dT%H%M%SZ)
vault_container=$(compose ps -q vault)
[ -n "$vault_container" ] || fail "the current vault container is unavailable"
vault_image_id=$(docker inspect --format '{{.Image}}' "$vault_container")
vault_rollback_tag="clipboard-vault:rollback-$timestamp"
database_snapshot="/data/backups/pre-upgrade-$timestamp.db"

say "Writing a consistent database snapshot to $database_snapshot (inside the data volume)"
compose exec -T vault /usr/local/bin/clipboard-vault backup "$database_snapshot" ||
  fail "the database snapshot failed; nothing was changed"
docker image tag "$vault_image_id" "$vault_rollback_tag"
write_state "$PENDING_STATE_FILE" "$previous_commit" "$target_commit" \
  "$vault_rollback_tag" "$database_snapshot"

say "Checking out exact commit $target_commit"
git checkout --detach "$target_commit"
if ! compose config --quiet; then
  git checkout --detach "$previous_commit"
  fail "the target Compose configuration is invalid; the previous container is still running"
fi

say "Building the new image while the current container keeps serving"
build_status=0
if [ "$MODE" = "refresh" ]; then
  compose build --pull vault || build_status=$?
else
  compose build vault || build_status=$?
fi
if [ "$build_status" -ne 0 ]; then
  git checkout --detach "$previous_commit"
  fail "build failed; the previous container is still running and the snapshot is at $database_snapshot"
fi

say "Starting the newly built service"
startup_status=0
compose up -d --no-build --wait --wait-timeout 120 vault || startup_status=$?
if [ "$startup_status" -ne 0 ] || ! wait_for_readiness; then
  say "The new service failed readiness; beginning automatic rollback"
  restore_image "$previous_commit" "$vault_rollback_tag"
  fail "upgrade failed and the previous deployment was restored; snapshot retained at $database_snapshot"
fi

write_state "$STATE_FILE" "$previous_commit" "$target_commit" \
  "$vault_rollback_tag" "$database_snapshot"
rm -f "$PENDING_STATE_FILE"
say "Upgrade complete at $target_commit"
say "Rollback image and database snapshot retained; Nginx stayed running throughout"

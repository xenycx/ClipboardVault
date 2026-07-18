#!/usr/bin/env sh
set -eu

if [ ! -f .env ]; then
  echo "Create .env from .env.example first."
  exit 1
fi

set -a
. ./.env
set +a

if [ -z "${DOMAIN:-}" ]; then
  echo "DOMAIN is missing from .env"
  exit 1
fi

docker compose up -d postgres auth vault nginx
docker compose --profile certificates run --rm certbot certonly \
  --webroot --webroot-path /var/www/certbot \
  --domain "$DOMAIN" \
  --agree-tos --register-unsafely-without-email --no-eff-email
docker compose -f docker-compose.yml -f docker-compose.prod.yml up -d --force-recreate nginx
echo "HTTPS is ready at https://$DOMAIN"


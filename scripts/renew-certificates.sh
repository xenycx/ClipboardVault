#!/usr/bin/env sh
set -eu
docker compose --profile certificates run --rm certbot renew --webroot --webroot-path /var/www/certbot
docker compose -f docker-compose.yml -f docker-compose.prod.yml exec nginx nginx -s reload


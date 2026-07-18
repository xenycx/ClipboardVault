#!/usr/bin/env sh
set -eu
base_url="${1:-http://localhost}"
echo "Health endpoint:"
curl -fsS -o /dev/null -w 'status=%{http_code} time=%{time_total}s\n' "$base_url/health/live"
echo "Run a real authenticated load test with oha:"
echo "oha -n 1000 -c 50 -H 'X-API-Key: YOUR_KEY' '$base_url/api/v1/items?limit=50'"

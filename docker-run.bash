#!/usr/bin/env bash
set -ex

SCRIPT_DIR="$( cd "$(dirname "$0")" ; pwd -P )"
DB_PATH="$SCRIPT_DIR/db"

mkdir -p "$DB_PATH"

docker run \
  --name delegation-strategy \
  --user "$UID" \
  --rm \
  --volume "$DB_PATH:/usr/local/db" \
  --env "VALIDATORS_APP_TOKEN=$VALIDATORS_APP_TOKEN" \
  delegation-strategy ./scripts/clean-score-mainnet

docker run \
  --name delegation-strategy \
  --user "$UID" \
  --rm \
  --volume "$DB_PATH:/usr/local/db" \
  delegation-strategy ./scripts/score-post-process

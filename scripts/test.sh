#!/usr/bin/env bash
set -euo pipefail
set -x

#docker build --no-cache -t fast-hash-index .

# Ejecutar con rutas absolutas
docker run -it --rm \
  --user "$(id -u)":"$(id -g)" \
  -v "$PWD/in":"/ws" \
  -v "$PWD/out":"/out" \
  -w "/ws" \
  fast-hash-index \
    "estado.txt" "." \
    --exclude "estado.txt" \
    --exclude ".git/**" \
    --target "/out"



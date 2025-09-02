APP=fast-hash-index
USER=dperezcabrera
VER=1.0.0

# Construir
docker build -t $USER/$APP:$VER -t $USER/$APP:latest .

# Push
docker push $USER/$APP:$VER
docker push $USER/$APP:latest


# syntax=docker/dockerfile:1.4
FROM python:3.12-alpine

WORKDIR /app

# airlock MITMs outbound HTTPS, so pip needs to trust the airlock CA.
# The merged bundle lives at /etc/ssl/certs/ca-certificates.crt inside the
# VM and is passed in as a build secret (see docker-compose.yml).
RUN --mount=type=secret,id=airlock_ca \
    pip install --no-cache-dir --cert /run/secrets/airlock_ca \
        flask==3.1.2 valkey==6.1.1

COPY app.py .

CMD ["python", "app.py"]

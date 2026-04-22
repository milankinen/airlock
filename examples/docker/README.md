# Docker in airlock

Minimalistic example how to run a full Docker engine inside the
sandbox as an airlock daemon and make (dockerized) app port
available in the host machine.

See the [user manual](../../docs/manual/src/tips/docker.md) for
more details.

## 1. Build sandbox image

First you need an image that has full docker installed. Build it
locally by running:

```bash
docker build -t airlock-example:docker -f sandbox.dockerfile .
```

## 2. Start sandbox

Then start the daemon.

```bash
airlock start -- docker compose up
```

Now you can open (host) browser at http://localhost:8000 to see
the dockerized app.

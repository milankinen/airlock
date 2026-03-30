# Design doc

Implementation details

## Virtualization

Works with MacOS and Linux

- Linux: `libkrun`
- MacOS: Apple Virtualization framework

## High level architecture

- Podman inside minimalistic alpine busybox rootfs
  1. CLI configures virtio devices and launches VM
  2. VM launches hypervisor that connects to host CLI app
     with vsock + capnproto 
  3. hypervisor launches daemonless podman container and connects 
     to it and forwards std between podman and CLI app using
     vsock 

## Networking

1. Hypervisor starts a http(s) proxy server and configures
   podman to use it as http(s) proxy for the container.
2. This proxy delegates requests to te CLI app using vsock
3. CLI applies network policies and injection to requests/responses
4. CLI sends responses back using vsock
5. Proxy sends responses back to container

## Memory

- Uses virtio balloon device
  - VM image contains virtio driver that is able to reclaim memory to
    host
  - CLI polls (or by some other trigger?) VM (using vsock?) to reclaim
    the unused memory

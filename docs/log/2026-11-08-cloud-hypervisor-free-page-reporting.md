# Free page reporting for the Cloud Hypervisor backend

The Cloud Hypervisor VM did not give memory back to the host. After a
large build or a big tmpfs file, the host process kept its highest
memory use until the VM stopped. The libkrun backend already used free
page reporting.

## Change

cloud-hypervisor starts with `--balloon size=0,free_page_reporting=on`.
The guest kernel reports free pages (`CONFIG_VIRTIO_BALLOON` selects
`CONFIG_PAGE_REPORTING`, on x86_64 and arm64). cloud-hypervisor then
removes the pages from the guest memory. The guest memory is a shared
memfd (`shared=on`, for the virtiofsd vhost-user devices), and
cloud-hypervisor punches holes in the file, so the memory also goes out
of the virtiofsd mappings.

The balloon has size 0 and never inflates. The guest can always use all
of its memory again. Nothing on the host controls the balloon.

## Measurement

2 GB VM, 900 MiB file in the guest tmpfs, then `rm`. Host RSS of
cloud-hypervisor:

| Step              | Without balloon | With balloon |
|-------------------|-----------------|--------------|
| Idle              | 196 MiB         | 193 MiB      |
| File written      | 1091 MiB        | 1092 MiB     |
| ~6 s after `rm`   | 1091 MiB        | 202 MiB      |

The guest reports free pages in 2 MiB blocks, with a short delay, so
small and scattered frees go back later or not at all. The guest page
cache stays in use: only free pages go back.

## Test

`tests/vm/memory.bats` (Linux only) fills 800 MiB of the guest tmpfs,
removes the file, and checks that the host RSS of cloud-hypervisor goes
down by 600 MiB or more.

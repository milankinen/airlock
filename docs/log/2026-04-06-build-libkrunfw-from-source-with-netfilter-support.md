# Build libkrunfw from source with netfilter support

The stock libkrunfw kernel has `CONFIG_NETFILTER` disabled, so iptables
rules for the network proxy (transparent TCP redirect to port 15001)
silently failed. Built libkrunfw from source in Docker with netfilter
config additions (nf_tables, iptables, conntrack, NAT redirect).

The build script now builds both libkrunfw (kernel) and libkrun (VMM) in
Docker containers from pinned versions (libkrunfw v5.3.0, libkrun v1.17.4).
Netfilter options are in a separate `netfilter.cfg` file appended to the
libkrunfw kernel config before compilation.

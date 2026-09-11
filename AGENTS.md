# Challenge

Build a gateway (this repo, running on the Pi) that bridges an unmodified PQ
meter onto a SCION network, without requiring changes to the meter. The
goal is a standardized, vendor-agnostic bridge — not a one-off adapter for a
single device.

Topology: PQ meter -- Ethernet --> gateway (Pi) -- LAN/WLAN --> laptop running
PocketSCION (SCION simulator), via its SNAP interface.

# IP Addresses
The PI IP is `10.175.8.132`

The PQ meter has the address `10.10.0.2`, only reachable by a wired connection on the hardware box

# Credentials
Username for the ssh login is `anapaya` - password is same

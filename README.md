# Alvin's Rusty Valve
A homemade forwarder and reverse proxy.
Untested, unproven, yet now running in the wild.
(famous last words)

**DON'T USE THIS IN PRODUCTION OR I'LL TELL YOUR MUM!**

## Current features:
- Transparent reverse proxy through peeking for
TLS Server Name Indication or HTTP Host header
- INI config with some command line options

## To do's:
- [x] Make an actual load balancer
- [ ] Connect SSH tarpit to the forwarders
(comes with free Bee Movie script)
- [ ] Finish gatekeeper
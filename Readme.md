# NSProxy

Kernel-namespaces-based alternative to proxychains.

> Part of Accessible OPSEC series (if there even is).

## Usage 

It's recommended to use the veth + tun2proxy method. The TUN2proxy and its tcp stack isn't well optimized. It acts as a compatibility layer for non-socks5-supporting apps.

```bash
sudo ./setsuid.sh # set sproxy to be SUID
sproxy veth -t ./test_proxy.json # gives you a shell inside a proxied container
# later you may
sproxy node <index> run # enter that container from another shell
```

## Rationale

- Firefox and its derivatives, leak traffic even with SOCKS5 proxy configured
    - Browsers in general have a lot of telemetry, which is unacceptable for what this project is trying to do.
- Proxychains may silently fail and leak traffic (but it's a great tool if you put it in a netns which nullfies the downsides)
- Nsproxy creates containers which better suits the OPSEC use case than [sing-box](https://github.com/SagerNet/sing-box)
- [Tun2socks](https://github.com/xjasonlyu/tun2socks) does not have virtual DNS
- VPNs (in the sense the binaries VPN vendors distribute) do not care about the OPSEC usecase. 

## The usecase

- You use non-conventional protocols. You need userspace TUNs.
- You have a diverse need for proxied routing, and you don't want to read a ton of docs.
- You want to have some apps proxied, and others not.
- You don't want to mess with other parts of your system. 
- You want to proxy Flatpak apps.

## We've got you covered

The proxy 

- If your proxy client is opensourced, it can be made to accept a socket from nsproxy
    - Nsproxy will create a container and you can access the proxy through a SOCKS5 endpoint in the container.
- If your proxy is opensourced and has custom TUN logic, it can be made to accepet the TUN file descriptor from nsproxy
- If your proxy can not be modified, you can use the `socks2tun` subcommand to connect to its SOCKS5 endpoint.

The app

- If your app doesn't work with SOCKS5
    - If your app works with LD_PRELOAD, you don't need a TUN.
        - You may use proxychains inside an Nsproxy container
        - Nsproxy creates a SOCKS5 endpoint in the container
    - Nsproxy may create a TUN and pass it to the proxy
    - Nsproxy may create a TUN and route it to the proxy's SOCKS5 endpoint
- If your app works with SOCKS5
    - You just connect to the SOCKS5 endpoint in the container

## Fix flatpak networking, sideways.

You can run `nsproxy watch ./test_proxy.json` to automatically proxy flatpak apps.

Currently it's not recommended (bad for anonymity) to have multiple instances of an app because the data could not be segregated, see [the issue](https://github.com/flatpak/flatpak/issues/1170).


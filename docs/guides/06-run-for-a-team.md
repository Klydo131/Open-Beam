# 6. Run Open Beam for a team

Open Beam is a single long-running program. To share it, run it on a machine
that stays on and put HTTPS in front of it.

## What kind of hosting works

| Hosting | Suitable | Why |
| --- | --- | --- |
| A computer or server you control | Yes | Runs the `beam` program directly. |
| A virtual machine from any cloud provider | Yes | Same as above. |
| A container platform that runs long-lived services | Yes | Build the binary into a container image. |
| Serverless or edge function platforms | No | They run short-lived functions and cannot host a long-running native program with an embedded database engine. |
| A static website host | No | Open Beam is a server, not a set of files. |

A tunnel or reverse-proxy service can sit in front of any of the suitable
options to provide the HTTPS address.

> These are general requirements. Open Beam has not yet been tested on
> specific hosting providers; reports of what works are welcome.

## Step 1: Require a password

Follow [Protect a feed with a password](05-protect-a-feed.md). Open Beam
refuses to listen on a network address without one.

## Step 2: Put HTTPS in front

Keep Open Beam listening on `127.0.0.1` and let a reverse proxy on the same
machine accept HTTPS and forward to it. For example, with
[Caddy](https://caddyserver.com), which obtains certificates automatically:

```text
beam.example.org {
    reverse_proxy 127.0.0.1:8787
}
```

## Step 3: Tell Open Beam its public address

So that the links it sends to Excel and Power BI are correct, set in
`beam.toml`:

```toml
public_url = "https://beam.example.org"
```

## Step 4: Keep it running

Use your operating system's service manager (systemd on Linux, Task Scheduler
or a service wrapper on Windows) to start Open Beam at boot with
`BEAM_PASSWORD` set in the service's environment.

## Where the data lives

Open Beam does not include or host a database. It reads the files you list,
from wherever you keep them. Reading directly from PostgreSQL and SQLite
databases that you operate is on the [roadmap](../ROADMAP.md).

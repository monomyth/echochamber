# Ephemeral relays

echochamber can start one relay per stream and throw it away when the stream ends. Nothing in the relay's streaming path changes. A deployment is a process or a VM that runs the same binary.

Three providers ship in this version:

| Order | Provider | Why it is here |
|---|---|---|
| 1 | Hetzner Cloud | Egress is the bill that matters. A planning figure for a typical relay hour is about 3 cents. The API is small. |
| 2 | Amazon Lightsail | A snapshot instance is often accepting RTMP in one to two minutes. |
| 3 | This computer | No cloud bill. The stream lives only as long as the machine and its link do. |

Fly.io is the likely next provider. A stopped machine there wakes in under a second, which is attractive for a relay, and outgoing bandwidth is billed, which is why it is not in this set. The provider trait is the place to add it. GCP and EC2 are not in this version.

## What you run

From a checkout, after `cargo build --release`:

```bash
# Hetzner. Token stays on this machine. See below.
export HETZNER_API_TOKEN=...
./target/release/echochamber deploy \
  --provider hetzner \
  --region fsn1 \
  --ttl-secs 14400 \
  --idle-secs 600

# Lightsail. Uses the AWS SDK default credential chain. No keys in the request.
./target/release/echochamber deploy \
  --provider aws-lightsail \
  --region us-east-1 \
  --image echochamber-snapshot \
  --prebuilt \
  --ttl-secs 14400

# This computer. Free. Home internet can drop, and a sleeping laptop kills the stream.
./target/release/echochamber deploy --provider local --region local
```

The command blocks until TCP 1935 accepts. It then prints JSON with `ingest_server` (the OBS server field) and `stream_key`. Those two fields are empty until that check passes.

Other commands:

```bash
./target/release/echochamber deployments
./target/release/echochamber teardown --id ec0123abcd01
./target/release/echochamber reap --provider hetzner --region fsn1 --dry-run
./target/release/echochamber reap --provider hetzner --region fsn1
```

While `echochamber serve` is running, the same actions are HTTP on the control port (localhost only, unless you changed the bind):

| Request | What it does |
|---|---|
| `GET /cloud` | Deploy page. The button calls the routes below. |
| `POST /deploy` | Create, wait, return the ingest URL. |
| `GET /deployments` | List relays recorded on this machine. |
| `GET /deployments/{id}` | One relay. |
| `POST /deployments/{id}/teardown` | Destroy it. |
| `POST /reap` | Sweep labeled orphans. Body: `{"provider":"hetzner","region":"fsn1","dry_run":true}`. |

The page and the CLI do not take cloud credentials. A JSON body that contains a token or a secret key is rejected.

If this machine's `config.toml` has YouTube, X, or Twitch stream keys, they are copied into the relay config so it can forward. YouTube OAuth client secrets are not copied. The copied keys sit in cloud user-data, which anyone who can read the cloud API can see. They are not written to the log.

## Ready means both checks

The controller does not return an ingest URL until the ingest port accepts a TCP connection.

On a cloud VM the control API is bound to `127.0.0.1` and requires a per-deployment bearer token. The cloud firewall opens only the ingest port (TCP 1935, optionally limited to a CIDR). The guest additionally drops port 1935 until `curl` to `http://127.0.0.1:8080/status` returns the relay JSON (`"publishing"` is in the body). A TCP accept from the outside therefore happens only after that local control check. The guest also drops port 8080 for every interface except loopback.

On this computer the controller itself calls both: TCP to the ingest port, and HTTP `/status` with the deployment token.

The cloud relay's control server is started with `ECHOCHAMBER_DISABLE_DEPLOY=1`, so that process cannot provision more machines.

## Time limits

Every cloud resource is labeled or tagged:

- `echochamber-managed=true`
- `echochamber-deployment-id`
- `echochamber-created-at`
- `echochamber-expires-at` (unix seconds)

The hard maximum lifetime is 24 hours. The default is 4 hours. The guest also shuts itself down when the deadline hits, when nobody connects within the connect grace (default 15 minutes), or when a stream that had started stays unpublished for the idle window (default 10 minutes). A second `sleep` until the deadline calls `shutdown` even if the idle unit never starts.

`shutdown` on a cloud guest stops that guest. It does not run on your laptop. The local watchdog kills only the relay process.

Stopped guests still cost money until they are deleted. `echochamber reap` deletes labeled resources whose deadline has passed, whose deadline tag is missing, or that are already stopped, terminated, or failed. It will not touch a resource that lacks `echochamber-managed=true`.

Lightsail's guest shutdown stops the instance; the reaper deletes it. Hetzner deletion removes the server and its firewall. Primary IPs created with the server go away with it. IPv6 is left off. No SSH key is installed, and port 22 is not opened.

## Hetzner

Create a Cloud project that contains only these relays. A token is project-wide. Hetzner does not offer per-action IAM. Least privilege here means:

1. A project with no other servers.
2. A token whose permission is **Read & Write**. Read cannot create or delete. There is no smaller scope.
3. The token lives in `HETZNER_API_TOKEN` or `HCLOUD_TOKEN`, or in the active context of `~/.config/hcloud/cli.toml`. It is never a field in the deploy request and it is not committed.

Default location is whatever you pass as `--region` (`fsn1`, `nbg1`, `hel1`, `ash`, `hil`, `sin`). Default type is `cx23`. Default image is `ubuntu-24.04`, which installs ffmpeg and echochamber on first boot. That install is slow. A snapshot you built once, passed as `--image` and `--prebuilt`, skips it.

The firewall is created first and attached at server create. Inbound is deny-by-default once a firewall is attached. The only inbound rule is TCP 1935 from `--ingest-cidr` (default `0.0.0.0/0`). Outbound stays open so the relay can push to YouTube, X, and Twitch.

### One-click path

The deploy page at `http://127.0.0.1:8080/cloud` is the one-click path: pick Hetzner, press **Deploy relay**. The token must already be on the machine.

Hetzner does not offer a public URL that prefills user-data, image, and firewall. A console "Deploy" button would need a Hetzner App listing or a console change. Until then, the paste path is:

```bash
./target/release/echochamber cloud-init --provider hetzner --ttl-secs 14400 > /tmp/user-data.sh
```

The script contains a stream key. Do not commit it. In the Hetzner console, create a firewall with one inbound TCP 1935 rule, create a server from your snapshot (or Ubuntu 24.04), paste the script into user data, and attach the firewall. You still have to delete that server yourself, or point `reap` at the project. The in-guest deadline will power the VM off; the server object remains until something deletes it.

`HCLOUD_ENDPOINT` overrides the API base. Leave it unset to use `https://api.hetzner.cloud/v1`.

## Lightsail

Credentials come from the AWS SDK default chain on this machine: environment variables, web identity, the shared config and credentials files, SSO, a credential process, container credentials, then IMDS. The deploy request cannot carry them.

Attach `deploy/iam/aws-lightsail-policy.json` to the IAM user or role that runs the control app. Create is allowed only when the request sets `echochamber-managed=true`. Delete, describe, and port changes are allowed only when that tag is already on the instance. List and catalog calls cannot be tag-scoped; Lightsail requires `Resource: "*"` for those. The policy does not grant EC2, S3, or IAM.

```bash
aws iam create-policy \
  --policy-name EchochamberLightsailDeploy \
  --policy-document file://deploy/iam/aws-lightsail-policy.json
```

`--region` is the Lightsail region. The zone defaults to `{region}a`. `--image` is a blueprint id (`ubuntu_22_04` by default) or, with `--prebuilt`, a snapshot name. `--size` is a bundle id (`small_3_0` by default).

After create, the only public port is TCP 1935. The default SSH port from the Ubuntu blueprint is replaced, not left open. The same guest script as Hetzner holds 1935 closed until the local control check passes.

### Transfer allowance is an open question

Do not decide Lightsail's cost by assuming either of these:

- an instance that lives about an hour receives a prorated share of the monthly data transfer allowance, or
- that hour consumes the whole monthly allowance.

Which one AWS bills is not settled here. Confirm it with AWS billing support before you commit to Lightsail for relays where egress is the cost. This tree does not compute a Lightsail transfer price.

## This computer

`echochamber deploy --provider local` starts another echochamber process, health-checks it, and returns an ingest URL on your LAN address when it can see one. Control stays on `127.0.0.1` with a fresh token. The parent checks both the RTMP port and `/status` before it prints the URL.

The process is detached. Idle, connect grace, and the hard deadline kill that process only. `reap --provider local` does the same for leftovers.

This is a free tier with no availability. Home internet drops. A laptop that sleeps takes the stream with it. Most home routers do not forward port 1935, so a device on the public internet cannot reach the relay unless you have arranged that yourself. A machine on the same LAN can.

State for local relays is under the echochamber config directory (`.../echochamber/local/<id>`), mode 0600. The deployment index is `deployments.json` next to it, also mode 0600. Override with `ECHOCHAMBER_DEPLOY_STATE` and `ECHOCHAMBER_LOCAL_STATE`. `ECHOCHAMBER_RELAY_BIN` chooses the binary; otherwise it is the current executable.

## Still required before a real cloud relay

- A Hetzner project and a Read & Write token, or an AWS account with the Lightsail policy above.
- Quotas: Hetzner server and firewall limits, Lightsail instance limits in the region.
- A snapshot or image with echochamber and ffmpeg if you do not want the first boot to compile from source. The fallback `cargo install` can take most of the ready timeout (default 10 minutes, max 30).
- DNS and TLS if you need RTMPS. This deploy path hands out `rtmp://` on port 1935. It does not request certificates or open 443.
- Billing alarms on the cloud account. The reaper is a command you run (or cron). It is not a cloud bill cap.
- Metering of who streamed is not in this version. The relay's existing `/status` bandwidth fields still work on the guest; they are not shipped to a billing system.
- Destination stream keys in your local `config.toml` if the cloud relay should forward. Without them it accepts ingest and pushes nowhere.

## Tests

`cargo test` uses an in-memory provider and a local fake of the Hetzner API. It does not create cloud resources and does not need tokens.

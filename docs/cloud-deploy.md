# Ephemeral relays

echochamber can start one relay per stream and throw it away when the stream ends. Nothing in the relay's streaming path changes. A deployment is a process or a VM that runs the same binary.

| Provider | What it is for |
|---|---|
| Hetzner Cloud | Cheapest egress. A planning figure for a typical relay hour is about 3 cents. |
| Amazon EC2 | The AWS VM. Guest shutdown terminates the instance. |
| Google Compute Engine | The GCP VM. The platform deletes it at `maxRunDuration`. |
| Amazon Lightsail | Snapshot boot in a minute or two. It cannot delete itself. |
| This computer | No cloud bill. The stream lives only as long as the machine and its link do. |

Fly.io is the likely next provider. A stopped machine there wakes in under a second, and outgoing bandwidth is billed. The provider trait is the place to add it.

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

# EC2. Keys from this device, or omit them and use the AWS SDK default chain.
./target/release/echochamber deploy \
  --provider aws-ec2 \
  --region us-east-1 \
  --image ami-0123456789abcdef0 \
  --aws-access-key-id AKIA... \
  --aws-secret-access-key ... \
  --ttl-secs 14400

# Compute Engine. The JSON file stays on this device.
./target/release/echochamber deploy \
  --provider gcp \
  --region us-central1 \
  --gcp-service-account-file ~/Library/echochamber/relay.json \
  --ttl-secs 14400

# Lightsail. Same AWS keys, or the SDK default chain.
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

`POST /deploy`, `POST /reap`, and `POST /deployments/{id}/teardown` accept optional credentials for that one call:

```json
{
  "provider": "aws-ec2",
  "region": "us-east-1",
  "image": "ami-0123456789abcdef0",
  "aws_access_key_id": "AKIA...",
  "aws_secret_access_key": "...",
  "aws_session_token": "..."
}
```

```json
{
  "provider": "gcp",
  "region": "us-central1",
  "gcp_service_account_json": "{ ...service account key... }"
}
```

An iOS app reads those from the Keychain and sends them. Echochamber does not write them to `deployments.json` or the log. Omit them and the AWS SDK default chain, or GCP Application Default Credentials, is used instead. Unknown fields, including a Hetzner token, are rejected. The Hetzner token stays in the environment on the machine that runs the command.

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

What happens when the phone disappears:

| Provider | Idle or deadline with no controller |
|---|---|
| EC2 | The instance is launched with `InstanceInitiatedShutdownBehavior=terminate`. The guest's `shutdown` deletes the instance. The security group has no instances left; `reap` removes it the next time a controller is online. An empty security group is not billed. |
| Compute Engine | `maxRunDuration` is the hard deadline and `instanceTerminationAction` is `DELETE`, so Compute Engine deletes the VM and its boot disk even if the guest watchdog never runs. The guest also shuts down on idle. A VM that has already stopped is no longer billed for CPU; `reap` deletes a stopped disk if the phone comes back before `maxRunDuration` finishes. The guest is not given a credential it could use to call the API itself. |
| Lightsail | Lightsail cannot self-delete. Guest `shutdown` stops the instance, and a stopped Lightsail instance keeps its charge until `teardown` or `reap` deletes it. |
| Hetzner | Guest `shutdown` powers the server off. The server object remains until `teardown` or `reap` deletes it. |

No SSH key is installed, and port 22 is not opened. Hetzner primary IPs go away with the server. IPv6 is left off there. Compute Engine also denies 22 and 3389 for the relay's network tag, ahead of the default allow-ssh rule.

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

## EC2

Create a user or role that can only touch tagged relays, then an access key for the phone:

```bash
aws iam create-policy \
  --policy-name EchochamberEc2Deploy \
  --policy-document file://deploy/iam/aws-ec2-policy.json
aws iam create-user --user-name echochamber-phone
aws iam attach-user-policy \
  --user-name echochamber-phone \
  --policy-arn arn:aws:iam::ACCOUNT:policy/EchochamberEc2Deploy
aws iam create-access-key --user-name echochamber-phone
```

Put the access key, secret, and any session token in the iOS Keychain. The app sends them on each deploy, teardown, and reap. On a development machine you can omit the flags and use the AWS SDK default chain instead: environment variables, web identity, shared config, SSO, a credential process, container credentials, then IMDS.

`deploy/iam/aws-ec2-policy.json` allows `RunInstances` on the instance, volume, and network interface only when the request tag `echochamber-managed` is `true`. Terminate and security-group changes require that tag on the resource. Describe calls cannot be tag-scoped. The policy does not grant S3 or IAM.

`--image` is an AMI id (`ami-...`) in `--region`. The zone defaults to `{region}a` and must have a default subnet. `--size` defaults to `t3.small`. The security group allows TCP 1935 from `--ingest-cidr` and nothing else. Metadata requires IMDSv2. No instance profile is attached.

## Compute Engine

Create a custom role and a service account, then a key the phone will hold:

```bash
gcloud iam roles create echochamberRelay \
  --project PROJECT \
  --file deploy/iam/gcp-custom-role.json
gcloud iam service-accounts create echochamber-phone --project PROJECT
gcloud projects add-iam-policy-binding PROJECT \
  --member serviceAccount:echochamber-phone@PROJECT.iam.gserviceaccount.com \
  --role projects/PROJECT/roles/echochamberRelay
gcloud iam service-accounts keys create /tmp/echochamber-phone.json \
  --iam-account echochamber-phone@PROJECT.iam.gserviceaccount.com
```

Move `/tmp/echochamber-phone.json` into the Keychain or a device-only file and delete the temporary copy. Do not commit it. Pass it with `--gcp-service-account-file`, or as `gcp_service_account_json` on the HTTP request. For a development machine, omit it and use Application Default Credentials (`GOOGLE_APPLICATION_CREDENTIALS`, `gcloud auth application-default login`, or the metadata server).

The role in `deploy/iam/gcp-custom-role.json` can create and delete instances, disks, and firewalls, and read images and operations. It cannot administer IAM. The VM is created with an empty access scope, so the guest cannot call Google APIs. `--region` is a Compute Engine region such as `us-central1`; the zone defaults to `{region}-a`. `--size` defaults to `e2-small`. `--image` defaults to the Ubuntu 24.04 image family.

## Lightsail

Credentials are the same AWS keys as EC2, or the SDK default chain when you omit them.

Attach `deploy/iam/aws-lightsail-policy.json` to the IAM user or role. Create is allowed only when the request sets `echochamber-managed=true`. Delete, describe, and port changes are allowed only when that tag is already on the instance. List and catalog calls cannot be tag-scoped; Lightsail requires `Resource: "*"` for those. The policy does not grant EC2 instance APIs, S3, or IAM.

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

- A Hetzner project and a Read & Write token, an AWS key for the EC2 or Lightsail policy, or a GCP service account for the custom role. The phone keeps that secret in the Keychain.
- Quotas: Hetzner servers and firewalls, EC2 vCPU and Elastic IP, Compute Engine CPUs and in-use addresses, Lightsail instance limits.
- A snapshot or image with echochamber and ffmpeg if you do not want the first boot to compile from source. The fallback `cargo install` can take most of the ready timeout (default 10 minutes, max 30).
- DNS and TLS if you need RTMPS. This deploy path hands out `rtmp://` on port 1935. It does not request certificates or open 443.
- Billing alarms on the cloud account. The reaper is a command you run (or cron). It is not a cloud bill cap.
- Metering of who streamed is not in this version. The relay's existing `/status` bandwidth fields still work on the guest; they are not shipped to a billing system.
- Destination stream keys in your local `config.toml` if the cloud relay should forward. Without them it accepts ingest and pushes nowhere.

## Tests

`cargo test` uses an in-memory provider and localhost fakes of the Hetzner and Compute Engine APIs. EC2 is tested through a fake of its launch and destroy calls. Nothing in the tests creates a cloud resource or needs a real key.

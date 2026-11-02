# aws-testbox — a throwaway EC2 box for hardware the lab does not have

Some field tests need a machine nobody owns: [FR-122](../../docs/fr/FR-122-arm64-linux-hardware-cell.md)
needs **arm64 Linux with an NVIDIA encoder**, and the rentable one is an AWS **g5g** (Graviton2 + T4G). These
scripts rent one for an hour or two and leave nothing behind. A test box gets a VPC of its own, SSH only from
the addresses you name, an SSH key made for that run, and a power-off deadline that terminates it even if
nobody runs `down.sh`.

| Script | Runs on | What it does |
|---|---|---|
| `up.sh` | your machine | VPC, subnet, internet gateway, security group, then the instance. Idempotent: a re-run resumes from the state file |
| `down.sh` | your machine | deletes exactly the ids the state file lists, then sweeps the region for the box's tag and says what is left. `--sweep` also deletes what the sweep finds |
| `ssh.sh` | your machine | ssh to the box with the run's key and `known_hosts` |
| `guest-roomlerd.sh` | the box | installs the **released** roomlerd as a root service with a virtual desktop, enrolls it, and reads its encoder probe, sessions and a recording |
| `lib.sh` | — | shared by the three above |

## Before the first run

- AWS CLI v2.32 or later, which has `aws login`. It gives the CLI short-lived credentials for the console
  session you pick in the browser, so no access keys are created:
  `aws login --profile <name> --region <the console's region>`. Start it in the region your console session
  is in; a sign-in started in another region fails with "Something went wrong".
- `ssh`, `ssh-keygen`, `curl`. Git-Bash on Windows works (the scripts turn its path rewriting off).
- Nothing defaults the account or the region. `AWS_PROFILE` and `AWS_REGION` are required on every call.

### Accounts on AWS's "new experience" (measured 2026-10-29)

An account opened through the new sign-up (a *project* under AWS Settings, on the free plan) refuses this
box three ways. Each refusal is explicit:

| Refusal | What you see | The way out |
|---|---|---|
| The organisation's pre-configured SCP pins every regional call to the project's region (`eu-north-1` for a European address) | `UnauthorizedOperation … with an explicit deny in a service control policy` on any call elsewhere | upgrade to the paid plan, *Activate advanced features* (irreversible; it also removes the spend limit), then add the region to the `RegionFloor` statement |
| The free plan launches free-tier types only (`t3.micro/small`, `t4g.micro/small`, `c7i-flex.large`, `m7i-flex.large`) | `InvalidParameterCombination: The specified instance type is not eligible for Free Tier` | the paid plan |
| G and VT instances start at a quota of 0 vCPUs (on-demand `L-DB2E81BA`, spot `L-3819A6DF`) | `up.sh` stops before launching and prints the request command | `aws service-quotas request-service-quota-increase --service-code ec2 --quota-code L-DB2E81BA --desired-value 4` |

`eu-north-1` offers no arm64 GPU type at all; `g5g` is in `eu-central-1`, among others. `up.sh` names the
zone that offers the type, or says that none does.

## A run

```bash
export AWS_PROFILE=<profile> AWS_REGION=eu-central-1 TESTBOX_NAME=fr122
scripts/aws-testbox/up.sh            # g5g.xlarge + AWS's NVIDIA driver image, 4 h deadline
scripts/aws-testbox/ssh.sh 'cat > guest-roomlerd.sh && chmod +x guest-roomlerd.sh' < scripts/aws-testbox/guest-roomlerd.sh
scripts/aws-testbox/ssh.sh './guest-roomlerd.sh prep && ./guest-roomlerd.sh install'
# … enroll, test (below) …
scripts/aws-testbox/down.sh          # exits 0 only when nothing tagged roomler-testbox=fr122 is left
```

| Knob | Default | |
|---|---|---|
| `TESTBOX_NAME` | `testbox` | the state directory (`~/.roomler-testbox/<name>`) and the tag value |
| `TESTBOX_TYPE` | `g5g.xlarge` | a free-tier rehearsal: `t4g.small` (the same Graviton2) |
| `TESTBOX_AMI` | `nvidia` on a GPU type, else `ubuntu` | `nvidia` = AWS's *Deep Learning Base OSS Nvidia Driver GPU AMI* (driver and NVENC libraries installed; Ubuntu 24.04 first, plain where the region has it and else *Ubuntu Pro*, whose licence bills a few cents an hour); `ubuntu` = Canonical's 24.04; or an `ami-` id |
| `TESTBOX_TTL_MIN` | `240` | minutes until the box powers itself off; its shutdown behaviour is *terminate* |
| `TESTBOX_SSH_CIDRS` | this machine's public IPv4 `/32` | comma-separated; add an orchestrator's address |
| `TESTBOX_SSH_PUBKEYS` | — | a file of extra public keys to authorise, such as a per-run key made on the orchestrator |
| `TESTBOX_SPOT` | `0` | `1` = a one-time spot instance (its own quota) |
| `TESTBOX_DISK_GB` | `40` | raised to the image's own size when that is bigger |

What `up.sh` builds, and why it is safe to forget about:

- **Its own VPC** (`10.199.0.0/24`), never the account's default or a shared one: deleting it proves nothing is
  left behind, and the box never joins a network it should not.
- **Port 22 only, from the listed sources.** The agent needs no inbound port: it dials the server, and media
  goes over ICE or a relay.
- **A key made for the run.** `ssh-keygen` writes it into the state directory and cloud-init authorises its
  public half. AWS never holds the private key, and `down.sh` deletes it.
- **A deadline inside the box.** A systemd timer powers it off at `EXPIRES`, catching up after a reboot; the
  instance then terminates and its encrypted root volume with it. Every resource carries
  `roomler-testbox=<name>` and `expires=<deadline>`.
- **IMDSv2 only**, and an encrypted gp3 root volume deleted on termination.

## The agent on the box

`guest-roomlerd.sh` installs the release the way a user does: `install.sh --role daemon --system --no-enroll
--no-desktop`. It then gives the root daemon a desktop of its own, because a cloud box has no screen:
`ROOMLERD_VIRTUAL_DESKTOP=1`, so Xvfb at 1920×1080 with openbox, a ticking clock and `top`. `ROOMLERD_AUTO_UPDATE=0`
pins the build under test.

The enrollment key arrives on **stdin**, one line, so it never sits in a command line on the machine that sends
it. For roomler's own field tests the key is the vmtest org's ephemeral key, which never leaves mars. Mars sends
it straight to the box, with a per-run key mars made for itself:

```bash
roomler ssh mars 'set -a; . ~/vmtest/.env; set +a; printf "%s\n" "$VMTEST_EPHEMERAL_KEY" |
  ssh -i ~/fr122/box_key ubuntu@<ip> "./guest-roomlerd.sh enroll <name> $VMTEST_SERVER"'
```

An ephemeral device deletes its own row when the daemon stops, so `guest-roomlerd.sh stop`, or the box's
termination, cleans the org too.

| Subcommand | Shows |
|---|---|
| `caps` | the daemon's `caps probe:` lines, including `child reported … hw_encoders=[…]`, the probe's matrix run again (`roomlerd caps-probe`), and the cache |
| `smoke` | `encoder-smoke --encoder hardware`: an offline open of the hardware encoders, with no session |
| `record hardware\|auto\|software` | a ten-second recording of a private Xvfb with `--system-audio` (a PulseAudio or PipeWire null sink). It prints the recorder's events (`started … "encoder":"h264_nvenc"`), the MP4's sample entries (`avc1`; `mp4a` and `esds` for AAC) and the sidecar, then deletes the file. `hardware` refuses on a host without a hardware H.264 encoder rather than recording in software |
| `logs [REGEX] [N]` | the daemon's own log, filtered by regex: `encoder selected`, `media pump`, the DC pump heartbeat |

The viewer for a session is the vmtest remote-desktop spec (`ui/e2e/vmtest-remote.spec.ts`), run in mars's
Playwright container against the agent's exact name. `E2E_RC_CODEC=h264` makes the session request one cell
(the codec picker's per-agent override), and `E2E_RC_EXPECT_ENCODER=h264_nvenc` fails the run unless the
viewer's codec pill names that encoder.

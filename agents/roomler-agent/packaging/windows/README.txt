Roomler Agent — README
======================

The Roomler AI remote-control agent runs on this machine, maintains an
outbound WebSocket connection to the Roomler API, and (on demand) serves
a WebRTC peer directly to a browser controller. All screen video and
input events travel on that P2P channel — the Roomler server never sees
them.

Installation
------------
This installer placed `roomler-agent.exe` at:

    %LOCALAPPDATA%\Programs\roomler-agent\roomler-agent.exe

No admin rights were required (per-user install, no UAC).

Configuration is stored under `%APPDATA%\roomler-agent\` once you run
`roomler-agent enroll`.

First-time setup
----------------
Open PowerShell (no need to run as administrator) and run:

    $agent = "$env:LOCALAPPDATA\Programs\roomler-agent\roomler-agent.exe"

    # 1. Generate an enrollment JWT in the admin UI at
    #    http://roomler.ai/ (Admin -> Agents -> New agent).
    #    It is valid for 10 minutes and can only be used once.
    #
    # 2. Enroll this machine:
    & $agent enroll `
        --server http://roomler.ai/ `
        --token <paste-enrollment-jwt> `
        --name $env:COMPUTERNAME

    # 3. Run the agent (foreground — confirm capture + signalling work):
    $env:RUST_LOG = "roomler_agent=debug,webrtc=info"
    & $agent run

You should see log lines like:
    agent starting
    signalling connected
    awaiting session

At that point this machine appears (online) in the admin UI at
http://roomler.ai/ under Admin -> Agents. A controller can click
"Connect" to open a remote desktop session.

Autostart on logon (recommended)
--------------------------------
Once the interactive run works, register the agent to start on every
login:

    & $agent service install

Query the current state with `service status`; remove with
`service uninstall`. The install subcommand creates a Scheduled Task
called `RoomlerAgent` with an ONLOGON trigger + LIMITED run level
(equivalent to the `schtasks /Create /SC ONLOGON` command below but
with a stable task name the agent can query + remove on its own).

Manual equivalent (if you'd rather drive it yourself):

    schtasks /Create /SC ONLOGON /TN "RoomlerAgent" `
             /TR "$agent run" /RL LIMITED

    schtasks /Delete /TN "RoomlerAgent" /F

Auto-update
-----------
On every start and every 6 hours, the agent checks GitHub Releases
for a newer version. If one is available, it downloads the MSI and
spawns `msiexec` to install it, then exits so the installer can
overwrite the binary. The Scheduled Task registered by
`service install` relaunches the new version on the next login.

Disable for air-gapped deployments with:

    setx ROOMLER_AGENT_AUTO_UPDATE 0

Manual check without installing:

    & $agent self-update --check-only

Note on privileges
------------------
The agent intentionally runs un-elevated:

 * Windows UIPI blocks input injection into elevated windows from a
   non-elevated process. That is by design — a connected controller
   cannot take over a UAC prompt on this machine.
 * DXGI screen capture requires an interactive user session (it cannot
   run as a Windows service in session 0).

If you want the controller to interact with an elevated window, restart
that window un-elevated, or use the Windows Security Attention Sequence
(Ctrl+Alt+Delete) on the local keyboard yourself.

Uninstall
---------
Settings -> Apps -> Installed apps -> Roomler Agent -> Uninstall.
Or from PowerShell:

    msiexec /x {product-code-here}

Logs
----
The agent writes to stderr. To capture a log file for troubleshooting:

    & $agent run 2>&1 | Tee-Object -FilePath "$env:TEMP\roomler-agent.log"

Set `RUST_LOG=roomler_agent=debug,webrtc=debug` for verbose output.

Support
-------
Project:  https://roomler.ai/
Source:   https://github.com/gjovanov/roomler-ai

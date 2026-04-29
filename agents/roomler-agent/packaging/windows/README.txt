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

Autostart on logon (registered automatically since 0.1.54)
----------------------------------------------------------
The MSI registers the auto-start hook automatically on install + on
every upgrade. After running the MSI you should already have a
Scheduled Task called `RoomlerAgent` with the resilience XML
(restart-on-failure, battery-defeat, single-instance belt). Confirm
with:

    schtasks /Query /TN RoomlerAgent

If you ever need to manage the hook manually (e.g. after restoring a
backup or recovering from the rare Win11 ACL-locked-task scenario):

    & $agent service install     # register / re-register
    & $agent service uninstall   # tear down
    & $agent service status      # query state

Earlier versions (0.1.49 and below) used a simpler `schtasks /Create
/SC ONLOGON` registration without the resilience settings — those
upgrade automatically once the new MSI lands. If your existing task
has an ACL tightened so even the owner can't overwrite it (rare
Win11 quirk), the MSI's RegisterAutostart custom action will silently
skip (Return="ignore" so install completes), and the operator can
delete the locked task from an elevated PowerShell once with:

    schtasks /Delete /TN RoomlerAgent /F

then re-run `service install` from a normal shell.

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

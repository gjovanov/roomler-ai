# Roomler Setup for Linux

Run `./roomler-setup` from this folder. It installs the Roomler service, or
the tunnel client, and joins this computer to your organization.

## It needs WebKitGTK 4.1

Roomler Setup draws its window with WebKitGTK 4.1, and not every desktop has
it. Without it `./roomler-setup` stops before its window opens. From a
terminal it says:

    error while loading shared libraries: libwebkit2gtk-4.1.so.0: cannot open shared object file

Opened from the file manager, nothing appears at all: that line goes only to the
system journal. Install it, then run `./roomler-setup` again:

- Ubuntu and Debian: `sudo apt install libwebkit2gtk-4.1-0`
- Fedora: `sudo dnf install webkit2gtk4.1`

## Or install from a terminal

The one-line install needs nothing installed first. Create a token under
**Devices → Enroll device** on your Roomler server, then:

    curl -fsSL https://roomler.ai/api/setup/install.sh | sh -s -- \
      --role daemon --token <token> --server https://roomler.ai --name "$(hostname)"

That installs the service for this user. Add `--system` for a root service,
as on a headless server. For your own server, put its address in both places.

All the ways to install: https://roomler.ai/docs/start/install/linux/

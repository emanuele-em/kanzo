# Podman Quadlet for Kanzo

[Quadlet](https://docs.podman.io/en/latest/markdown/podman-systemd.unit.5.html) manages Podman containers as systemd services.

## Installation

### Rootless (Recommended)

1. Copy the unit files to your user systemd quadlet directory:

   ```sh
   mkdir -p ~/.config/containers/systemd
   cp quadlet/kanzo.container quadlet/kanzo-data.volume ~/.config/containers/systemd/
   ```

2. Reload systemd and start Kanzo:

   ```sh
   systemctl --user daemon-reload
   systemctl --user enable --now kanzo
   ```

3. View logs or check status:

   ```sh
   systemctl --user status kanzo
   journalctl --user -u kanzo -f
   ```

### System-wide (Rootful)

Copy the unit files to `/etc/containers/systemd/` and run `systemctl daemon-reload && systemctl enable --now kanzo`.

## Configuration

- **Expose to LAN:** By default, `PublishPort=127.0.0.1:4242:4242` keeps the board accessible only from the local machine. To share it on your local network, change it to:
  ```ini
  PublishPort=4242:4242
  ```

- **Bind Mount to `~/kanzo`:** To use the standard `~/kanzo` directory on the host instead of a named volume:
  ```ini
  [Container]
  ...
  Volume=%h/kanzo:/data:Z
  UserNS=keep-id
  ```

- **Automatic Updates:** The unit includes `AutoUpdate=registry`. Enable automated container updates with:
  ```sh
  systemctl --user enable --now podman-auto-update.timer
  ```

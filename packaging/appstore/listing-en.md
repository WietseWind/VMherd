# VMherd: Mac App Store listing (English, U.S.)

| Field | Value |
|---|---|
| Name | VMherd |
| Subtitle | Console grid for Proxmox VE |
| Primary category | Developer Tools |
| Secondary category | Utilities |
| Price | EUR 19.99 (base: Netherlands) |
| Support URL | https://vmherd.app/support/ |
| Marketing URL | https://vmherd.app |
| Privacy policy URL | https://vmherd.app/privacy/ |
| Copyright | 2026 The Integrators BV |
| License agreement | Apple's standard EULA |
| Age rating | 4+ (no objectionable content, no web browsing, no user-generated content) |
| App Privacy | Data Not Collected |
| Encryption | Only Apple OS encryption (ITSAppUsesNonExemptEncryption = NO in Info.plist) |

## Promotional text (151/170)
Every Proxmox VE console, live in one native Mac window. Type once into all of them, or click one to go solo. Token auth, Keychain secrets, pinned TLS.

## Keywords (99/100 bytes)
vnc,kvm,qemu,lxc,container,hypervisor,virtual machine,homelab,sysadmin,devops,cluster,server,remote

## Description (3972/4000)
VMherd puts the live consoles of your Proxmox VE virtual machines and containers in one native Mac window, and lets you type into all of them at once, or into just one.

Made for sysadmins and DevOps engineers who install, rescue and bootstrap machines before SSH works: OS installers, first logins, network setup, boot menus, recovery shells.

TRY IT WITHOUT A SERVER
• A built-in demo cluster with 13 simulated VMs and containers shows every feature: live consoles, broadcast typing, placeholders and power buttons. It runs entirely inside the app.

ONE KEYBOARD, MANY MACHINES
• Broadcast: click the broadcast bar (ON AIR) and every keystroke goes to all synced consoles at the same time, as real key presses.
• A sync toggle on every tile decides which consoles follow along. Sync All or None in one click.
• Solo: click a console to type and use the mouse in that VM only.
• Type text: send a command or a script to every synced console, with an adjustable per-character delay for reliable input.
• Per-VM placeholders: {{vmid}}, {{name}}, {{node}}, {{ipv4}} and {{ipv6}} are filled in for each VM, so one line such as "hostnamectl set-hostname {{name}}" gives every machine its own value. Addresses come from the QEMU guest agent or the container's interfaces.
• Buttons for Enter, Tab, Esc, arrow keys, Ctrl+C, Ctrl+D, Ctrl+Z and Ctrl+L. In broadcast or solo mode, ⌘V types the clipboard and asks first when it holds several lines.
• Hold Esc for one second to stop; a short Esc goes to the consoles.

FIND AND PICK VMS FAST
• Filter by name, VMID, node, status or tag: a space means AND, a comma means OR. Paste a column of VMIDs to select exactly those.
• Running only, sortable columns (VMID, name, node, status, tags, uptime) and natural name order: web-2 comes before web-10.
• Click to add or remove, shift-click for a range, or stay on the keyboard: / opens the list, Tab and arrow keys move, Space picks.
• Tiles appear in the order you pick them. Automatic layout or up to 8 columns. Each cluster remembers its grid.

CONTROL EACH MACHINE FROM YOUR DESKTOP
• Every tile shows VMID, name and a status lamp above its live console, plus node and uptime when there is room.
• Start, shut down (ACPI) or hard stop a single VM without opening the Proxmox web interface. Shutdown and stop need a second click.
• Maximize any console to the full window. Consoles connect by themselves while a VM runs, so you see it boot, and reconnect after a drop.
• Consoles open in shared mode: a Proxmox web console that is already open stays connected.

MULTIPLE CLUSTERS
• Bookmark any number of Proxmox VE clusters or single nodes and switch between them from the top bar.

SECURE BY DESIGN
• API token authentication (user@realm!tokenname). You decide what the token may do: VM.Audit and VM.Console, plus VM.PowerMgmt for the power buttons.
• Token secrets are stored in your macOS Keychain, never in the settings file.
• HTTPS only for remote servers. Self-signed Proxmox certificates are pinned on first use by their SHA-256 fingerprint, like SSH host keys. A changed certificate is refused until you trust it again.
• No account, no analytics. VMherd connects only to the servers you add.

NATIVE AND FAST
• Written in Rust, with its own VNC client. No browser tabs, no plugins.
• Runs natively on Apple silicon and Intel Macs.

REQUIREMENTS
• A Proxmox VE server or cluster that your Mac can reach over HTTPS (usually port 8006).
• An API token with VM.Audit and VM.Console, plus VM.PowerMgmt for the power buttons.
• For {{ipv4}} and {{ipv6}} on VMs: the QEMU guest agent in the VM, and VM.Monitor (Proxmox VE 8) or VM.GuestAgent.Audit (Proxmox VE 9) on the token.
• Broadcast keystrokes follow the guest's own keyboard layout. "Type text" uses US key codes, so the guest needs a US layout for it.
• The app is in English.

Proxmox is a registered trademark of Proxmox Server Solutions GmbH. VMherd is not affiliated with or endorsed by Proxmox Server Solutions GmbH.

## App Review notes (2369/4000 bytes)
VMherd is a desktop client for Proxmox VE, the open-source virtualization platform that people run on their own servers. VMherd has no accounts and no backend of its own: users connect it to their own Proxmox servers with an API token.

To review it without a Proxmox server, VMherd has a built-in demo cluster. It is a normal, visible feature for every user:
1. Launch VMherd. On the start screen, click TRY THE DEMO (the "No Proxmox at hand?" card).
2. A grid of simulated consoles opens (13 VMs and containers in total). Everything runs inside the app: nothing connects anywhere and nothing is saved.
3. Broadcast input: press Return, or click the long bar at the bottom. It turns red and shows ON AIR. Type "uptime" and press Return: the keystrokes reach every tile whose red sync toggle is on. Hold Esc for one second to stop.
4. Per-VM values: in the text field under the bar, enter  echo {{name}} {{ipv4}}  and click TYPE. Each VM prints its own name and address.
5. Solo: click inside one console. It gets an amber SOLO label and only that VM receives keys and mouse input.
6. VM list: click VMS (top left), or press "/" when nothing is focused. Type "k8s" to filter; click rows to add or remove tiles.
7. Power: on a tile, click stop or shutdown (a second click confirms), then the play button to start it again.
8. Leave the demo from the "Demo cluster" menu at the top (Leave demo).

WHAT BROADCAST INPUT DOES
Keys typed while the VMherd window is focused and the bar shows ON AIR go to the selected consoles only, as VNC key events (over the Proxmox API for real clusters). VMherd uses no Accessibility, Input Monitoring or event-tap APIs and never sends keys to other apps.

ENTITLEMENTS
- com.apple.security.app-sandbox
- com.apple.security.network.client: HTTPS and secure WebSocket connections to the Proxmox VE servers the user adds (REST API and console stream, usually port 8006). VMherd opens no listening sockets and contacts no other servers.
Token secrets are stored as the app's own Keychain items. macOS may show the Local Network prompt because Proxmox servers are usually on the user's LAN.

Guideline 4.2.7 does not apply: VMherd shows each VM's whole console (a generic VNC view of the VM), not a mirror of specific software or services.

Proxmox is a registered trademark of Proxmox Server Solutions GmbH; VMherd is not affiliated with it.

## Screenshots (2880x1800, no alpha; generated by tools/store-shots/make.py)
1. Type once. Every console follows. (broadcast)
2. Every VM console. One window. (grid)
3. Type it once. Every VM gets its own. (placeholders)
4. Any VM. A few keystrokes away. (picker)
5. Or take just one. (solo)
6. Power buttons, right on the tile. (power)
7. One click to full screen. (maximized)

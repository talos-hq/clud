---
name: clud-dell-idrac
description: Recover authorized access to Dell PowerEdge or VxRail servers, inspect hardware through iDRAC/Redfish, and establish agent-controlled screen, keyboard, mouse, and installation media. Use for Dell server takeover, health reports, or provisioning, including used servers with unknown credentials.
triggers:
  - When recovering authorized access to a Dell PowerEdge or VxRail server
  - When inspecting Dell server health or configuring iDRAC agent console access
  - When provisioning a Dell server through Redfish, virtual media, or remote KVM
---
<!-- managed-by: clud -->

# Dell server access and agent control

Establish working remote control and report the machine's actual state. iDRAC
is independent of the host OS: a reachable login page does not prove that the
server is powered on, has storage, or can boot. A VxRail E560F is a rack
appliance; do not infer a blade chassis controller or motherboard jumper layout
from an approximate model name.
Preserve the requested scope: status-only diagnosis does not authorize reboot,
password rotation, firmware flashing, or disk erasure.

## Start with existing local evidence

If `~/.local/share/dell-control/inventory.json` exists, read it first. It is a
local connection profile, not a credential file. It may identify the address,
service tag, certificate fingerprint, keyring service/users, and existing
control scripts. Treat its observations as historical and refresh them live.
Do not put a customer's addresses, service tags, passwords, or screenshots in
the shared skill. If the operator names a NixOS/configuration checkout, inspect
its instructions and existing edits before proposing host/control-plane changes.

Match the chassis service tag to `/redfish/v1/` → `Oem.Dell.ServiceTag` and the
authenticated system inventory before sending input or power commands. Use
existing inventory/DHCP information before scoped discovery on an authorized LAN.
Redfish requests need `Accept: application/json`; some iDRAC firmware otherwise
returns HTTP 406. Follow resource links rather than assuming OEM paths across
firmware generations: on iDRAC9 4.2x, `DellLCService` and the license
collection live under `/redfish/v1/Dell/Managers/iDRAC.Embedded.1/...`
(listed in the manager resource's links); on 4.40 they are under
`/redfish/v1/Managers/iDRAC.Embedded.1/Oem/Dell/...`. A 404 on one form is
not a missing feature.

For a new iDRAC on the LAN, the unauthenticated service root
(`GET /redfish/v1/`) carries `Oem.Dell.ServiceTag` and `ManagerMACAddress`;
a bounded sweep of the authorized /24 for it identifies every iDRAC without
logging in. Pin the HTTPS certificate's SHA-256 at first contact and check it
before every credentialed request. The iDRAC VNC TLS endpoint presents the
same certificate.

## Acquire or recover access

Use the existing authorized credential sink/catalog first. With clud's secret
tools available, follow `/clud-secret`; never print credentials, paste them into
chat, or place real passwords in command arguments. For a recorded KDE Wallet /
Secret Service profile, retrieve only the named service/user inside the client
process. A locked/unavailable desktop wallet in a headless Hermes session is a
credential-access problem, not evidence that the server needs resetting. Provide
that runtime an authorized secret sink; do not copy the desktop wallet wholesale.

For an owned used server without a known login:

- Check purchase instructions/seller correspondence if accessible, then the
  pull-out information tag, including its underside. A MAC address is not a
  password. Do not stop merely because an email connector is unavailable.
- Check the exact model's Dell manual for factory credentials. Some VxRail
  models document username `root`, password `calvin`; others use a unique tag
  password. If appropriate, make one deliberate documented factory-login test
  before declaring a credential blocker. Do not run a password-guessing loop.
- If that fails, Dell documents local recovery even when the old password is
  lost: VGA display + USB keyboard, boot **F2 → iDRAC Settings → User
  Configuration → Change Password**, save. Local configuration or a BIOS setup
  password can restrict this path. Confirm the exact chassis manual before any
  physical password-jumper procedure; do not guess pins from another model.
- On VxRail, change the account password rather than factory-resetting all
  iDRAC settings: the OEM configuration supports hardware monitoring. An
  iDRAC restart/system-ID-button reset is not a password override.

When replacing a factory password within the authorized setup, persist the new
credential securely before changing the server and verify a fresh login before
discarding the recovery route. Generated secrets go directly to the approved
keystore/sink without appearing in transcripts or repositories. The verified
order: generate in-process → store in the keyring → read back → `PATCH
AccountService/Accounts/<id>` `{"Password": ...}` with the old login → log in
fresh with the new one (expect 200) → confirm the old one now returns 401.
Re-running after a partial failure reuses the stored value rather than
generating another. A VxRail E640 (PowerEdge R640 base, iDRAC 4.22) bought
used accepted Dell's documented factory login on the one deliberate attempt.

## Establish both observation and input

Check the installed license via the manager's Dell license collection.
Enterprise/Datacenter enables iDRAC9 VNC/virtual-console features; verify the
license is valid, not just present. Inspect the actual console and virtual-media
resources. Do not buy external capture hardware before testing built-in control.

For screen observation, discover this action on `DellLCService`:

```text
POST /redfish/v1/Managers/iDRAC.Embedded.1/Oem/Dell/DellLCService/Actions/DellLCService.ExportServerScreenShot
{"FileType":"ServerScreenShot"}
```

Base64-decode `ServerScreenShotFile` into a local PNG and view it; never dump the
image payload. This is screen observation only, not keyboard/mouse control.

For headless agent input, use authenticated, encrypted VNC or a compatible
virtual-console client. Query the live manager attribute registry before
changing `VNCServer.1.*` settings. On firmware 4.22 and 4.40, VNC uses a
separate eight-character password and supports a TLS tunnel before the RFB
handshake. The verified configuration used `SSLEncryptionBitLength =
"256-Bit or higher"` and TLS 1.2 cipher `AES256-GCM-SHA384`; a client with
default cipher settings fails that handshake with `HANDSHAKE_FAILURE`, so pin
the cipher rather than lowering the server's setting. VNC ships disabled; one
`PATCH Managers/iDRAC.Embedded.1/Attributes` setting `Enable`, `Password`,
`SSLEncryptionBitLength` and `Timeout` enables it without an iDRAC reset.
Other firmware may differ. Verify the
certificate/recorded fingerprint before sending credentials; do not silently
accept a changed fingerprint or fall back to cleartext VNC. Hold key/button
presses briefly (about 120 ms) before releasing so USB emulation observes them.

If the local profile lists the verified `vnc_input.py` helper, inspect its
current interface first. It supports commands such as:

```bash
uv run --no-project --python-preference only-managed --script "$HOME/.local/share/dell-control/vnc_input.py" CAPTURE /tmp/dell-console.png
uv run --no-project --python-preference only-managed --script "$HOME/.local/share/dell-control/vnc_input.py" DOWN /tmp/dell-after-key.png
uv run --no-project --python-preference only-managed --script "$HOME/.local/share/dell-control/vnc_input.py" CLICK 95 219
```

Those coordinates are examples, not a fixed BIOS layout: view the current
screen first. Verify input by observing a harmless selection/menu change, not
just a successful socket write. A blank frame may be a BIOS display transition;
refresh the screenshot and inspect power/boot state before restarting anything.

If built-in access cannot be recovered, external KVM-over-IP needs **both**
VGA → active VGA-to-HDMI converter → capture **and** USB keyboard/mouse
emulation into the Dell. Capture alone and ordinary host-to-host USB cables
cannot provide input. Physical cabling remains a human task.

## Inspect before provisioning

Collect model/service tag, power state, CPU/RAM, BIOS/iDRAC versions, license,
storage controllers/drives/volumes, PSU input/redundancy, temperatures/fans,
host-network link, and active faults. Follow `Managers/.../LogServices` links;
do not clear logs to make health appear green. Distinguish unavailable readings
while powered off, historical inventory, active faults, and observed boot errors.

If an operator reports a server "screaming", read every fan's RPM and health
under `Chassis/.../Thermal` before blaming load or the thermal profile. One
failed rotor (0 RPM, `Fan redundancy is lost` in the SEL) makes iDRAC run
every other fan near maximum (about 20,000 RPM on an R640) even with cool
CPUs. No setting or offset safely overrides that fail-safe; the fix is
physical: reseat the hot-swap fan module, then replace it if the rotor still
reads 0. Report it, and power the server off only if the operator asks.

Do not label a machine healthy from a single rollup. For example, missing AC
on PSU 2 may make overall health critical while CPUs/temperatures are normal.
No bootable OS plus empty drive/volume collections means storage needs checking;
it does not prove every physical bay is empty. On a server that has just
received AC, the PERC's drive collection stays empty until the first POST
completes; power on and re-read before diagnosing missing disks. A used VxRail
typically arrives with its BOSS card removed (boot entries "Unavailable:
VMware ESXi") and its vSAN disks still in the front bays as `Ready`
unconfigured drives; prior data may remain on them even with no virtual disks
or foreign configuration, so inspect them read-only before any erase. Report what the API and console
actually establish, and check controller/BIOS evidence before diagnosing hardware.

For an authorized installation, establish the target OS and disk erase scope,
verify usable storage, then attach an installer through discovered VirtualMedia
actions and set a one-time boot target. Check HTTP errors **and** JSON `error`
and extended-info fields; some firmware returns an application error with HTTP
200. Re-read boot state before rebooting. iDRAC9 4.x mounts HTTP virtual media
only from port 80; serve the image read-only on a LAN address and watch the
web server's log for the iDRAC's range requests as proof it is reading. A
staged BIOS or controller job runs during the next POST ("System
Configuration Requested by Lifecycle Controller") and adds minutes, and a
one-time `BootSourceOverrideTarget: Cd` survived that pass on iDRAC 4.22.
The override is consumed by the boot that uses it, so re-arm it before any
further installer reboot. An installer with your public key baked in is
reachable over ssh as soon as it leases an address; find it by the server's
NIC MACs (`EthernetInterfaces`) in the neighbour table. A live image creates
new ssh host keys on every boot, so replace its known-hosts entry after a
reboot, not the installed host's.

A PERC H740P in RAID personality hides `Ready` disks from the OS. Check
`storcli64 /c0 show personality` before assuming firmware limits: firmware
50.9.4 already listed eHBA. `set personality=eHBA` writes nothing to the disks
and applies at the next reboot. Before erasing, sample the disks read-only
(`wipefs -n`, `sfdisk -d`, nonzero bytes at offsets across the disk); erase
only what the authorized scope covers, and report what was found. Verify boot, persistent networking,
and SSH/OS administration afterward. iDRAC access does not unlock encrypted
disks. Do not claim installation, virtual-media boot, or a firmware upgrade was
tested merely because its API is advertised.

Deliver a short status report with verified controls, faults, remaining physical
requirements, and the non-secret local profile/tool locations. Separate control
readiness from a completed OS installation. For code fixes discovered during
this workflow, preserve RED -> GREEN with a focused failing reproduction before
the fix and a passing rerun afterward.

## Authoritative references

- [Dell: forgotten-password local recovery](https://www.dell.com/support/kbdoc/en-us/000177787/how-to-change-the-default-login-password-of-the-idrac-9)
- [Dell: VxRail factory-reset monitoring warning](https://www.dell.com/support/kbdoc/en-us/000043420/vxrail-resetting-idrac-to-default-settings-breaks-hardware-monitoring-functionality)
- [Dell: virtual console and license requirements](https://www.dell.com/support/kbdoc/en-us/000179797/dell-poweredge-idrac-virtual-console)
- [Dell: screenshot Redfish implementation](https://github.com/dell/iDRAC-Redfish-Scripting/blob/master/Redfish%20Python/ExportServerScreenShotREDFISH.py)
- [Dell: iDRAC9 4.x security guide, VNC section](https://dl.dell.com/topicspdf/idrac9-lifecycle-controller-v4x-series_administrator-guide_en-us.pdf)

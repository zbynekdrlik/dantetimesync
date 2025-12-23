# Dante Time Sync - Target Nodes

The following nodes must be updated and verified after every successful CI/CD run.

## Active Targets

| Hostname | IP | OS | User | Password | Status |
|---|---|---|---|---|---|
| develbox | 10.77.9.21 | Linux | newlevel | - | v1.5.3 LOCKED |
| iem | 10.77.9.231 | Windows | iem | iem | v1.5.3 NANO |
| ableton-foh | 10.77.9.230 | Windows | ableton-foh | newlevel | v1.5.3 NANO |
| mbc.lan | 10.77.9.232 | Windows | newlevel | newlevel | v1.5.3 LOCKED (USB NIC) |
| strih.lan | 10.77.9.202 | Windows | newlevel | newlevel | v1.5.3 NANO |
| stream.lan | 10.77.9.204 | Windows | newlevel | newlevel | v1.5.3 PROD (high jitter) |

## Needs Console Access (Elevated Privileges)

| Hostname | IP | OS | User | Password | Issue |
|---|---|---|---|---|---|
| bridge.lan | 10.77.9.201 | Windows | newlevel | newlevel | Fresh install needed (run as admin) |
| songs | 10.77.9.212 | Windows | newlevel | newlevel | SSH access denied |

# The contract with JayOS

JayOS's shell starts this package's `jayos-bluetooth daemon`, runs its
`watch` from the Bluetooth page and the network popup, installs its BlueZ
`main.conf` from its installer, reports on its WirePlumber drop-ins from its
doctor, and builds this package with a recipe of its own -- all from another
repository. These files are what the two sides agree on, in a form each side
can test against without the other's code. JayOS vendors a copy of this
directory and of `share/`, records the release it came from and each file's
sha256, and holds its side to the copy -- a release's copy, since JayOS
builds the release its recipe pins and vendors the same one
(`docs/RELEASES.md`).

| File | What it is | Made by |
|---|---|---|
| `VERSION` | The schema of the files below: 1. A reader refuses a newer one. | hand |
| `verbs.txt` | The verbs of `jayos-bluetooth`, in its usage's order. JayOS may run these and no others. | generated |
| `ops.txt` | The ops the daemon answers, on its owner's stdin and on its socket, in the protocol table's order (`docs/BLUETOOTH.md`, *The protocol*). JayOS's page, popup and service may send these and no others. | generated |
| `steps.txt` | The verbs of `packaging/steps.sh` and their arguments: everything JayOS's recipe calls. | generated |
| `files.txt` | Every path the package installs, relative to `/`. JayOS reads it for where the drop-ins and `main.conf` are; `tests/shell/test-packaging.sh` holds `steps.sh package` to it exactly, both ways. | hand |

The generated files are made by `tools/contract.sh`, which reads the code
rather than describing it, and which fails when any of them differs from
what the code now says (`tools/check.sh` runs it). After an intended change:

    tools/contract.sh --write

then commit the result, make a release, and pin it in JayOS
(`tools/package-release.sh jayos-bluetooth <version>` there).

The `.txt` files carry a header of `#` lines; a reader skips them.

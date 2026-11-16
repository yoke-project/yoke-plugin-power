# Trying power

This directory is an application project that uses the Plugin in `..`: `yoke-compose init` made it and
`add plugin .. --yes` took the Plugin into it. Nothing here is the Plugin, and taking the Plugin into
another application never takes this directory.

| Path | What it is |
| --- | --- |
| `yoke.yaml` | the application: the unit `power` and the channel `panel`, a local socket for one client |
| `ui/` | the panel: an interface client written with Slint and the Rust interface library. It shows the supply, the battery and every device UPower knows, sets the profile, and says at once when the supply changes or the battery runs low. It reaches the Plugin through the contract alone and links none of its code |
| `follow/` | an interface client: it asks every question, follows every stream and every event of the unit, and prints each as one line |
| `run.sh` | runs the application on a local Core, grants what the Plugin declares, and opens the panel, or follows the unit with `--follow` |

```sh
./run.sh                        # the panel; closing it stops everything
./run.sh --read-only            # no command granted: the panel reads, and the Core refuses a profile
./run.sh --follow --seconds 10  # one line per answer, frame and event, in place of the panel
```

Unplug the supply while it runs to see `power.status` move and `power.supply.changed` reported. With
`--follow`, a command goes from another terminal, as `run.sh` says:

    echo '{"profile":"power-saver"}' | YOKE_INSTANCE=<instance> yoke unit command power power.profile.set --input -

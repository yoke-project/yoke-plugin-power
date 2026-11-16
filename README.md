# power

A Yoke Plugin that reads the host's power through UPower — whether it runs on battery, the battery's
charge and time left, every device UPower knows, a mouse's or a headset's battery among them — and sets
the power profile through power-profiles-daemon.

| | |
| --- | --- |
| questions | `power.status` (the supply, the battery, the profile, with its sequence), `power.devices`, `power.profiles` |
| commands | `power.profile.set` `{"profile": "power-saver" \| "balanced" \| "performance"}` |
| streams | `power.status`: every change, none lost; `power.devices`: every device, whole, at every change |
| occurrences | `power.supply.changed` (10 onto external power, 30 onto the battery), `power.battery.low` (50), `power.battery.critical` (70) |

Every payload is JSON and states `schema`; a question takes `{}`. Every occurrence carries its facts as
JSON in its detail — `line`, and the percentage, the level, the time left — which is what an interface
client receives.

**What is low and what is critical is the host's**: the occurrences follow UPower's warning level, which
the host's own settings decide, and are reported when it rises.

**An interface that attaches late** subscribes to `power.status`, then asks it, and drops what the stream
brings at or below the sequence it was answered with.

A host without power-profiles-daemon has a status whose `profile` is `null`, and `power.profiles` and
`power.profile.set` are failed as `not_found`. Setting a profile is polkit's to allow, apart from what the
operator granted.

Take it into an application with `yoke-compose add plugin https://github.com/yoke-project/yoke-plugin-power@<tag>`.
Build it with `yoke-compose build`, and try it from `try/`.

Licensed under the Apache License 2.0.

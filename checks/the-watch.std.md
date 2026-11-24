# The watch

| | |
| --- | --- |
| **Feature** | this Plugin against what Yoke last published |
| **Planning item** | yoke-project/yoke-plugin-power#1 |

## yoke-plugin-power:the-watch.01 — built and taken by the newest release of Yoke, the Plugin runs under its Core and answers its status

| Field | Value |
| --- | --- |
| **Cites** | specs/95.40 |
| **Level** | L3 |
| **Method** | check |
| **Not applicable in** | — |
| **Label** | blocking |
| **Precondition** | the newest published programs of Yoke; UPower or not |
| **Action** | build the Plugin, take it into `try/`, run it, grant `power.status` and ask it |
| **Expected** | the instance comes up; the question is answered with a sequence where UPower runs, and failed as `unavailable` where none does |

## yoke-plugin-power:the-watch.02 — the panel in `try/ui`, built against the published library, attaches to the instance with no screen

| Field | Value |
| --- | --- |
| **Cites** | specs/70.9 · arch/90-sdks/07 §What each one is |
| **Level** | L3 |
| **Method** | check |
| **Not applicable in** | — |
| **Label** | blocking |
| **Precondition** | the instance of `.01`, running; X under Xvfb, and Slint's software renderer |
| **Action** | build the panel in `try/ui` and run it on the application's channel `panel` of that instance |
| **Expected** | within 60 s the panel says *Attached* — the window it draws and its stderr say the same — so a Slint panel written with the Rust interface library opens a Session on the published contract and reads its opening |

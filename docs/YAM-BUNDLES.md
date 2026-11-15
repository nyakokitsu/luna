# Package-owned Lua pages and multi-plugin packages

Private proposed host change, based on official v0.2.26. Not deployed or published.
No AirPlay-specific endpoint, calibration UI or hardware change is included.

A YAM package can carry several Luna plugin directories in the same namespace.
For example `example/bundle` can own `example.receiver`, `example.command`
and `example.page`. Their permissions, enable/service state and process
lifecycles remain independent; their files have one package owner/version.

The host resolves plugin.toml to the canonical immutable payload:

```
$YAM_ROOT/pkgs/<namespace>/<package>/<version>/share/luna/plugins/<plugin-id>/plugin.toml
```

It verifies exact layout, matching plugin ID, matching namespace, a real plugin
root and current selector belonging to that package/version. The displayed
package/removal hint comes from this verified owner, not `plugin-id.replace`.
Launchers/entries still must stay inside the plugin root.

YAM Lua plugins now acquire a shared transaction lease and use the selected
payload root, just like binary plugins. That makes register.customweb work
without copying files into local Luna storage or weakening HTTP path containment.
Stop Lua services as well as binaries before YAM transactions. A failed Lua
start/stop releases the lease and removes its registered page.

Local .yap plugins remain supported. No auto-grant of web/exec/admin permissions,
no new listener and no package-manager implementation changes.

Tests: binary bundle component ownership, shared-package Lua page serving,
successive versions, page removal, transaction lock lifetime, foreign descriptor,
escaped entry/current, cross-namespace rejection, and existing binary permission
and runtime dependency tests. Full workspace validation is separately blocked
in the upstream v0.2.26 Station Max crate (missing StationSpec.led_calibration);
the existing Lite2 firmware assertion also fails in the broad library suite.
Neither is fixed as part of this package-hosting change.

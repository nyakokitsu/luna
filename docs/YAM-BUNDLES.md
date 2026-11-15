# YAM package-owned plugin bundles and Lua pages

One YAM package may own several Luna plugins in its namespace.
For example, `example/bundle` can ship:

```
share/luna/plugins/example.receiver/plugin.toml
share/luna/plugins/example.command/plugin.toml
share/luna/plugins/example.page/plugin.toml
```

They keep separate permissions, enable/service flags and process lifecycles,
but their files have one package owner, selected version and update/removal
transaction. This is a host capability, not a change to YAM's package format.
There is no plugin-specific API, bundled web page or new network listener.

## Verified ownership

Luna resolves the descriptor to the canonical immutable payload:

```
$YAM_ROOT/pkgs/<namespace>/<package>/<version>/share/luna/plugins/<plugin-id>/plugin.toml
```

It verifies exact layout, matching plugin ID and namespace, a real plugin root,
the selected package version and descriptor ownership. Launchers, entries and
private libraries must still stay inside their plugin root. A descriptor from
a different version or namespace is rejected.

The API package field and removal hint use that verified owner; they no longer
guess the package name by replacing the dot in each component's plugin ID.
Existing one-plugin packages keep working without manifest changes.

## Lua lifetime and web pages

YAM Lua plugins acquire a shared transaction lease and use the selected payload
root for execution and register.customweb. The lease lasts for the service's
whole lifetime. Pages therefore work through normal YAM file links without
copying HTML into local plugin storage or weakening HTTP path containment.
Stopping/failing Lua releases its lease and unregisters the page.

Stop Lua services as well as binary plugins before YAM transactions. Do not
delete yam.lock. Local .yap plugins remain supported. No web/exec/admin permission
is automatically granted; existing enable and binary-execution checks remain.

## Validation

Tests cover a binary component owned by a differently named bundle, a Lua page
in the same package as a binary component, actual HTML serving, successive
versions, page cleanup, transaction exclusion, foreign descriptors,
entry/current escapes, namespace rejection and existing runtime authorization.

The matching host change was also exercised with a private companion bundle in
full-system emulation: migration from a split companion, upgrade, corrupt input
rejection, removal and reinstall. Four physical bench units were updated; three
had manual cold-boot checks and one had normal reboot checks after installation.
That validates the package-hosting path, not independent LED calibration or
audio/colour quality.

On the current upstream v0.2.26 baseline, the Lite2 firmware assertion fails
unchanged; Station Max also lacks its StationSpec.led_calibration initializer.
Report these separately when running broad/workspace tests. This change does
not alter firmware validation or station hardware implementations.

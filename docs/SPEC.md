# Duck Packages v1 specification

## Product

Duck Packages (`io.github.srwalkerb.DuckPackages`) is a native GNOME utility
written in Rust with GTK4 and libadwaita. It lists applications installed by the
distribution, opens their real desktop launcher, exposes every distribution
package in an advanced view, and removes a selected package only after showing a
simulated transaction.

Official v1 packaging is an RPM for Fedora 44+ on x86_64 and aarch64. The
PackageKit contract remains portable to distributions backed by DNF, APT, or
Zypper. Flatpak, Snap, AppImage, installing, updating, repository management,
and removing user data are out of scope.

## Backend and safety

`BackendFactory` selects an implementation by runtime capability, never by
parsing the distribution name. `PackageKitBackend` is selected only when the
system D-Bus service is at least version 1.3.5 and exposes `GetPackages`,
`SearchFiles`, and `RemovePackages`. Otherwise `DiagnosticBackend` keeps the
application catalog readable while disabling destructive actions.

Package IDs are opaque outside the presentation helpers. Every removal uses
PackageKit `RemovePackages` with `allow_dependencies=false` and
`autoremove=false`. A simulated `RemovalPlan` is presented, simulated again
after confirmation, compared by requested IDs and the complete sorted removal
set, and executed in a separate transaction only if unchanged. Interactive
authorization is explicitly allowed on the execution call so PackageKit can
invoke Polkit.

## Catalog and interface

The catalog reads visible distribution-owned `.desktop` launchers through GIO,
enriches them with the local AppStream pool, and falls back to PackageKit
`SearchFiles` to find the owner. Hidden, `NoDisplay`, user, Flatpak, and Snap
launchers are omitted. Aliases are deduplicated using executable, startup WM
class, and normalized display name.

The root uses `AdwNavigationView`, an application grid, a shared search field,
and a lazily loaded virtualized `GtkColumnView` for packages. Details expose the
launcher, package identity, version, architecture, origin, installed size, and
description. The impact dialog is the only distinctive visual treatment: it
lists the exact removal set, freed space, affected applications, and restart
scope before offering the destructive action.

English is the source language and PT-BR is provided through gettext. Themes,
typography, accessibility, window chrome, semantic colors, focus, and reduced
motion follow GTK/libadwaita.

## Acceptance

- Chrome's visible and hidden desktop aliases yield one visible card.
- Launching uses `GAppInfo`; no shell or package-manager CLI exists in the code.
- Removal never occurs without simulation and confirmation.
- A plan that changes before execution returns `PlanChanged`.
- Missing, unsupported, insecure, or unavailable PackageKit produces diagnostic mode.
- The advanced view contains all installed distribution packages.
- Domain, catalog, safe flags, metadata, translations, formatting, linting, and
  the RPM build are validated in CI.

